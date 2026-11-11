//! Walk-in check-in tablets: the walk-in form runs on a paired tablet, with
//! no user account and no password (see [`crate::auth::check_in`]).
//!
//! - `GET    /v1/check-in/tablets`             → the company's paired tablets (admins)
//! - `POST   /v1/check-in/tablets/this-device` → this browser becomes a tablet; the admin is signed out
//! - `DELETE /v1/check-in/tablets/{id}`        → remove a tablet (admins)
//! - `GET    /v1/check-in/kiosk`              → the paired tablet's company (none when not paired)
//! - `GET    /v1/check-in/kiosk/duplicates`   → a customer who already has the phone or email
//! - `POST   /v1/check-in/kiosk/customers`    → save a walk-in

use crate::auth::AdminUser;
use crate::auth::check_in::{CHECK_IN_COOKIE, CheckInTablet, find_tablet, request_key};
use crate::auth::cookie::{SESSION_MAX_AGE_SECONDS, request_session_id};
use crate::auth::device::{new_token, token_hash};
use crate::auth::session::{find_session_user, user_positions};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::{IntoParams, ToSchema};
use crate::openapi::{OpenApiRouter, routes};
use crate::routes::company::queries::company_features;
use crate::routes::customers::ReferralSource;
use crate::routes::customers::duplicates::phone_last10;
use crate::routes::customers::service::{refuse_taken_phones, source_allowed, sync_emails};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::header::{CACHE_CONTROL, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, MySqlPool};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_tablets))
        .routes(routes!(this_device))
        .routes(routes!(remove_tablet))
        .routes(routes!(kiosk))
        .routes(routes!(kiosk_duplicates))
        .routes(routes!(kiosk_customer))
}

/// `positions.id` of the old check-in accounts (from before tablets).
const CHECK_IN_POSITION: i32 = 8;
/// A tablet's name when the admin gives none.
const DEFAULT_NAME: &str = "Walk-in tablet";
/// Longest tablet name kept.
const NAME_CHARS: usize = 100;
/// The source walk-ins are saved with.
const WALK_IN_SOURCE: &str = "check-in";

const ISO: &str = "'%Y-%m-%dT%H:%i:%sZ'";

/// A paired walk-in tablet.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct Tablet {
    /// Tablet id.
    pub id: i32,
    /// Its name.
    pub name: String,
    /// When it was paired (UTC).
    pub paired_at: Option<String>,
    /// When it last opened the form (UTC).
    pub last_seen_at: Option<String>,
    /// The admin who set it up (null for a tablet that was a check-in account).
    pub created_by_name: Option<String>,
}

/// Body of `POST /v1/check-in/tablets/this-device`.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewTablet {
    /// Its name, e.g. "Front desk" ("Walk-in tablet" when blank).
    #[serde(default)]
    pub name: Option<String>,
}

/// What a paired tablet shows.
#[derive(Debug, Serialize, ToSchema)]
pub struct Kiosk {
    /// The company whose walk-in form this is.
    pub company_id: i32,
    /// Its name.
    pub company_name: Option<String>,
    /// Its logo URL.
    pub logo_url: Option<String>,
    /// Its logo height in the header, in pixels.
    pub logo_height: Option<i32>,
    /// This tablet's name.
    pub tablet_name: String,
    /// The company texts customers (CloudTalk, RingCentral or Granite
    /// Manager texting): the form links the texting terms.
    pub sms_terms: bool,
}

/// Answer of `GET /v1/check-in/kiosk`.
#[derive(Debug, Serialize, ToSchema)]
pub struct KioskStatus {
    /// The paired tablet's form; null when this browser is not a paired
    /// tablet (the set-up screen shows).
    pub kiosk: Option<Kiosk>,
}

/// Query of `GET /v1/check-in/kiosk/duplicates`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct KioskDuplicatesQuery {
    /// Phone (its last 10 digits are compared).
    pub phone: Option<String>,
    /// Email, compared case-insensitively.
    pub email: Option<String>,
}

/// A customer who already has the walk-in's phone or email.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct KioskDuplicate {
    /// Their name.
    pub name: Option<String>,
    /// Their sales rep's name.
    pub sales_rep_name: Option<String>,
}

/// Body of `POST /v1/check-in/kiosk/customers`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WalkInInput {
    /// Name (required).
    pub name: String,
    /// Phone.
    pub phone: Option<String>,
    /// Email.
    pub email: Option<String>,
    /// Address.
    pub address: Option<String>,
    /// How they heard of the company.
    pub referral_source: Option<ReferralSource>,
}

/// Result of `POST /v1/check-in/kiosk/customers`.
#[derive(Debug, Serialize, ToSchema)]
pub struct SavedWalkIn {
    /// The new customer's id.
    pub id: i32,
}

fn blank_to_none(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

fn tablet_select(filter: &str) -> String {
    format!(
        "SELECT d.id, d.name,
                DATE_FORMAT(d.paired_at, {ISO}) AS paired_at,
                DATE_FORMAT(d.last_seen_at, {ISO}) AS last_seen_at,
                NULLIF(TRIM(u.name), '') AS created_by_name
           FROM check_in_devices d
           LEFT JOIN users u ON u.id = d.created_by
          WHERE {filter}"
    )
}

/// List walk-in tablets
///
/// The company's paired walk-in tablets, newest first.
#[crate::openapi::path(
    get,
    path = "/v1/check-in/tablets",
    operation_id = "list_check_in_tablets",
    tag = "check-in",
    responses(
        (status = 200, description = "Tablets", body = Vec<Tablet>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_tablets(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<Vec<Tablet>>> {
    let pool = &state.pool;
    let tablets = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{} ORDER BY d.id DESC",
        tablet_select("d.company_id = ? AND d.revoked_at IS NULL AND d.paired_at IS NOT NULL")
    )))
    .bind(user.company_id)
    .fetch_all(pool)
    .await?;
    Ok(Json(tablets))
}

/// Remove a walk-in tablet
///
/// The tablet no longer opens the walk-in form: it shows the set-up screen
/// again.
#[crate::openapi::path(
    delete,
    path = "/v1/check-in/tablets/{id}",
    operation_id = "remove_check_in_tablet",
    tag = "check-in",
    params(("id" = i32, Path, description = "Tablet id")),
    responses(
        (status = 204, description = "Removed"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such tablet in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn remove_tablet(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    let done = sqlx::query(
        "UPDATE check_in_devices
            SET revoked_at = UTC_TIMESTAMP(), token_hash = NULL
          WHERE id = ? AND company_id = ? AND revoked_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?;
    if done.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// What the tablet shows for its company.
async fn kiosk_for(pool: &MySqlPool, tablet: &CheckInTablet) -> ApiResult<Kiosk> {
    let (company_name, logo_url, logo_height): (Option<String>, Option<String>, Option<i64>) =
        sqlx::query_as(
            "SELECT name, NULLIF(NULLIF(TRIM(logo_url), ''), 'undefined'),
                    CAST(logo_height AS SIGNED) FROM company WHERE id = ?",
        )
        .bind(tablet.company_id)
        .fetch_optional(pool)
        .await?
        .unwrap_or((None, None, None));
    let features = company_features(pool, tablet.company_id).await?;
    Ok(Kiosk {
        company_id: tablet.company_id,
        company_name,
        logo_url,
        logo_height: logo_height.and_then(|h| i32::try_from(h).ok()),
        tablet_name: tablet.name.clone(),
        sms_terms: features.cloudtalk || features.ringcentral || features.telnyx_sms,
    })
}

/// A JSON answer that stores the tablet's key (again, so the browser keeps
/// it as long as it is used) and is never cached.
fn with_key(body: impl Serialize, state: &AppState, key: &str, extra: Vec<String>) -> Response {
    let mut response = Json(body).into_response();
    let mut cookies = extra;
    cookies.push(
        state
            .cookie
            .set_cookie(CHECK_IN_COOKIE, key, SESSION_MAX_AGE_SECONDS),
    );
    for cookie in cookies {
        if let Ok(value) = HeaderValue::from_str(&cookie) {
            response.headers_mut().append(SET_COOKIE, value);
        }
    }
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Make this device a walk-in tablet
///
/// Run on the tablet by an admin signed in there: this browser becomes a
/// paired walk-in tablet of the company (its key goes in the
/// `granite_check_in` cookie) and the admin is signed out of it at once, so
/// the CRM is never left open on the tablet. A browser that already was a
/// tablet drops its old pairing.
#[crate::openapi::path(
    post,
    path = "/v1/check-in/tablets/this-device",
    operation_id = "make_check_in_tablet",
    tag = "check-in",
    request_body = NewTablet,
    responses(
        (status = 200, description = "Paired; the tablet cookie is set and the session ended", body = Kiosk),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn this_device(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    headers: HeaderMap,
    JsonBody(input): JsonBody<NewTablet>,
) -> ApiResult<Response> {
    let pool = &state.pool;
    let name: String = blank_to_none(input.name.as_deref())
        .unwrap_or_else(|| DEFAULT_NAME.to_string())
        .chars()
        .take(NAME_CHARS)
        .collect();
    let key = new_token().map_err(|err| ApiError::Internal(err.to_string()))?;
    let id = sqlx::query(
        "INSERT INTO check_in_devices (company_id, name, token_hash, paired_at, last_seen_at, created_by)
         VALUES (?, ?, ?, UTC_TIMESTAMP(), UTC_TIMESTAMP(), ?)",
    )
    .bind(user.company_id)
    .bind(&name)
    .bind(token_hash(&key))
    .bind(user.id)
    .execute(pool)
    .await?
    .last_insert_id();
    // The same browser was a tablet before: that pairing ends (one row per tablet).
    if let Some(old_key) = request_key(&headers)
        && let Some(old) = find_tablet(pool, &old_key).await?
    {
        sqlx::query(
            "UPDATE check_in_devices SET revoked_at = UTC_TIMESTAMP(), token_hash = NULL WHERE id = ?",
        )
        .bind(old.id)
        .execute(pool)
        .await?;
    }
    // Signed out, as `POST /v1/auth/logout` does.
    if let Some(session_id) = request_session_id(&headers, &state.session_secret) {
        sqlx::query("UPDATE sessions SET is_deleted = 1 WHERE id = ?")
            .bind(&session_id)
            .execute(pool)
            .await?;
        crate::phone::revoke::revoke_web_session(
            &state,
            &session_id,
            crate::phone::revoke::RevokeReason::SignedOut,
        )
        .await?;
    }
    let tablet = CheckInTablet {
        id: i32::try_from(id).unwrap_or(i32::MAX),
        company_id: user.company_id,
        name,
    };
    let body = kiosk_for(pool, &tablet).await?;
    Ok(with_key(body, &state, &key, state.cookie.clear_session()))
}

/// A tablet still signed in with an old check-in account (from before
/// tablets were paired) becomes a paired tablet of the account's company:
/// its session ends, and the account is deleted once none of its sessions
/// is left. Nothing to set up: the account could already open the form.
async fn adopt_check_in_account(
    state: &AppState,
    headers: &HeaderMap,
) -> ApiResult<Option<(CheckInTablet, String)>> {
    let pool = &state.pool;
    let Some(session_id) = request_session_id(headers, &state.session_secret) else {
        return Ok(None);
    };
    let Some(user) = find_session_user(pool, &session_id).await? else {
        return Ok(None);
    };
    if user.is_admin || user.is_superuser {
        return Ok(None);
    }
    let positions = user_positions(pool, user.id).await?;
    let check_in_account = positions
        .iter()
        .any(|row| row.position_id == CHECK_IN_POSITION && row.company_id == user.company_id);
    if !check_in_account {
        return Ok(None);
    }
    let name = blank_to_none(user.name.as_deref())
        .filter(|name| !name.eq_ignore_ascii_case("check-in"))
        .unwrap_or_else(|| DEFAULT_NAME.to_string())
        .chars()
        .take(NAME_CHARS)
        .collect::<String>();
    let key = new_token().map_err(|err| ApiError::Internal(err.to_string()))?;
    let mut tx = pool.begin().await?;
    let id = sqlx::query(
        "INSERT INTO check_in_devices (company_id, name, token_hash, paired_at, last_seen_at)
         VALUES (?, ?, ?, UTC_TIMESTAMP(), UTC_TIMESTAMP())",
    )
    .bind(user.company_id)
    .bind(&name)
    .bind(token_hash(&key))
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    sqlx::query("UPDATE sessions SET is_deleted = 1 WHERE id = ?")
        .bind(&session_id)
        .execute(&mut *tx)
        .await?;
    let other_session: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM sessions WHERE user_id = ? AND COALESCE(is_deleted, 0) = 0 LIMIT 1",
    )
    .bind(user.id)
    .fetch_optional(&mut *tx)
    .await?;
    if other_session.is_none() {
        sqlx::query("UPDATE users SET is_deleted = 1 WHERE id = ? AND is_deleted = 0")
            .bind(user.id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    let tablet = CheckInTablet {
        id: i32::try_from(id).unwrap_or(i32::MAX),
        company_id: user.company_id,
        name,
    };
    Ok(Some((tablet, key)))
}

/// The walk-in tablet
///
/// The company, logo and texting terms the paired tablet's walk-in form
/// shows, and its key stored again. `kiosk` is null when this browser is no
/// (longer a) paired tablet. A browser still signed in with an old check-in
/// account becomes a paired tablet here (its session ends).
#[crate::openapi::path(
    get,
    path = "/v1/check-in/kiosk",
    operation_id = "get_check_in_kiosk",
    tag = "check-in",
    responses(
        (status = 200, description = "The tablet's form, or null", body = KioskStatus),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn kiosk(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    let pool = &state.pool;
    if let Some(key) = request_key(&headers)
        && let Some(tablet) = find_tablet(pool, &key).await?
    {
        let kiosk = kiosk_for(pool, &tablet).await?;
        return Ok(with_key(
            KioskStatus { kiosk: Some(kiosk) },
            &state,
            &key,
            Vec::new(),
        ));
    }
    if let Some((tablet, key)) = adopt_check_in_account(&state, &headers).await? {
        let kiosk = kiosk_for(pool, &tablet).await?;
        return Ok(with_key(
            KioskStatus { kiosk: Some(kiosk) },
            &state,
            &key,
            state.cookie.clear_session(),
        ));
    }
    let mut response = Json(KioskStatus { kiosk: None }).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

/// Find a walk-in's customer
///
/// The first live customer of the tablet's company with the phone (last 10
/// digits) or email, with their sales rep, for the form's "Possible
/// duplicate found" notice. Empty without either.
#[crate::openapi::path(
    get,
    path = "/v1/check-in/kiosk/duplicates",
    operation_id = "find_check_in_duplicates",
    tag = "check-in",
    params(KioskDuplicatesQuery),
    responses(
        (status = 200, description = "At most one customer", body = Vec<KioskDuplicate>),
        (status = 401, description = "Not a paired tablet", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn kiosk_duplicates(
    State(state): State<AppState>,
    tablet: CheckInTablet,
    Query(query): Query<KioskDuplicatesQuery>,
) -> ApiResult<Json<Vec<KioskDuplicate>>> {
    let phone = phone_last10(query.phone.as_deref());
    let email = blank_to_none(query.email.as_deref());
    if phone.is_none() && email.is_none() {
        return Ok(Json(Vec::new()));
    }
    let found = sqlx::query_as(
        "SELECT c.name, u.name AS sales_rep_name
           FROM customers c
           LEFT JOIN users u ON u.id = c.sales_rep AND u.is_deleted = 0
          WHERE c.deleted_at IS NULL AND c.company_id = ?
            AND ((? IS NOT NULL
                  AND (RIGHT(REGEXP_REPLACE(COALESCE(c.phone, ''), '[^0-9]', ''), 10) = ?
                       OR RIGHT(REGEXP_REPLACE(COALESCE(c.phone_2, ''), '[^0-9]', ''), 10) = ?))
                 OR (? IS NOT NULL
                     AND EXISTS (SELECT 1 FROM customers_emails m
                                  WHERE m.customer_id = c.id AND LOWER(m.email) = LOWER(?))))
          ORDER BY c.id LIMIT 1",
    )
    .bind(tablet.company_id)
    .bind(&phone)
    .bind(&phone)
    .bind(&phone)
    .bind(&email)
    .bind(&email)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(found))
}

/// Save a walk-in
///
/// Saves the visitor as a customer of the tablet's company with the
/// Walk-in source, no sales rep, and the tablet's name as who created it.
/// A phone or email of another customer is refused (409).
#[crate::openapi::path(
    post,
    path = "/v1/check-in/kiosk/customers",
    operation_id = "save_check_in_walk_in",
    tag = "check-in",
    request_body = WalkInInput,
    responses(
        (status = 201, description = "Saved", body = SavedWalkIn),
        (status = 400, description = "Missing name or short address", body = ErrorBody),
        (status = 401, description = "Not a paired tablet", body = ErrorBody),
        (status = 409, description = "A phone or email belongs to another customer", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn kiosk_customer(
    State(state): State<AppState>,
    tablet: CheckInTablet,
    JsonBody(input): JsonBody<WalkInInput>,
) -> ApiResult<(StatusCode, Json<SavedWalkIn>)> {
    let name = input.name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest("Name is required".into()));
    }
    let address = blank_to_none(input.address.as_deref());
    if address.as_ref().is_some_and(|a| a.chars().count() < 3) {
        return Err(ApiError::BadRequest(
            "Address must be at least 3 characters".into(),
        ));
    }
    let pool = &state.pool;
    if !source_allowed(pool, tablet.company_id, WALK_IN_SOURCE, None).await? {
        return Err(ApiError::BadRequest("Walk-ins are switched off.".into()));
    }
    let phone = blank_to_none(input.phone.as_deref());
    let emails: Vec<String> = blank_to_none(input.email.as_deref()).into_iter().collect();
    let created_by: String = tablet.name.chars().take(NAME_CHARS).collect();
    let mut tx = pool.begin().await?;
    refuse_taken_phones(&mut tx, tablet.company_id, 0, &[phone.as_deref()]).await?;
    let id = sqlx::query(
        "INSERT INTO customers (name, phone, address, referral_source, source, company_id, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(name)
    .bind(&phone)
    .bind(&address)
    .bind(input.referral_source.map(ReferralSource::as_str))
    .bind(WALK_IN_SOURCE)
    .bind(tablet.company_id)
    .bind(&created_by)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    let id = i32::try_from(id).unwrap_or(i32::MAX);
    sync_emails(&mut tx, id, &emails, None::<i32>).await?;
    tx.commit().await?;
    state.webhooks.sync_contacts(tablet.company_id, id).await;
    Ok((StatusCode::CREATED, Json(SavedWalkIn { id })))
}
