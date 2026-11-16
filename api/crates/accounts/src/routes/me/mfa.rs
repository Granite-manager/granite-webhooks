//! Two-step sign-in of the signed-in user (My Account › Security on the
//! web, Settings › Security in the app).
//!
//! - `GET  /v1/me/mfa`          → on or off, and whether the user must keep it on
//! - `POST /v1/me/mfa/setup`    → the QR code to add to an authenticator app
//! - `POST /v1/me/mfa/confirm`  → its first code: two-step sign-in is on
//! - `POST /v1/me/mfa/disable`  → a current code: two-step sign-in is off
//!
//! Takes the session cookie or the app's device token. Turning it on or
//! off signs no one out: it applies from the next sign-in.

use crate::auth::{CurrentUser, sha256_hex};
use crate::routes::auth::{client_ip, record_failed_login, too_many_failures};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::mfa::{self, AuthenticatorSetup};
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::header::CACHE_CONTROL;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// `GET /v1/me/mfa`.
#[derive(Debug, Serialize, ToSchema)]
pub struct MfaStatus {
    /// Two-step sign-in is on: every sign-in asks for the authenticator
    /// app's code after the password.
    pub enabled: bool,
    /// When it was turned on (ISO 8601, UTC); null while off.
    pub enabled_at: Option<DateTime<Utc>>,
    /// The user must keep it on (superusers): it cannot be turned off, and
    /// a sign-in without it set up asks to set it up first.
    pub required: bool,
}

/// Body of `POST /v1/me/mfa/confirm` and `POST /v1/me/mfa/disable`.
#[derive(Deserialize, ToSchema)]
pub struct MfaCode {
    /// The 6-digit code the authenticator app shows now (spaces ignored).
    pub code: String,
}

impl std::fmt::Debug for MfaCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MfaCode").finish_non_exhaustive()
    }
}

const WRONG_CODE: &str =
    "That code is not right. Check the code in your authenticator app and try again.";

/// Wrong codes of signed-in users count like failed sign-ins (10 per
/// address, 50 in all, per 15 minutes), keyed by the user.
async fn check_limits(state: &AppState, user_id: i32, headers: &HeaderMap) -> ApiResult<(String, String)> {
    let key = sha256_hex(&format!("mfa:{user_id}"));
    let ip = sha256_hex(&client_ip(headers));
    if too_many_failures(&state.pool, &key, &ip).await? {
        return Err(ApiError::TooManyRequests(
            "Too many wrong codes. Please try again later.".into(),
        ));
    }
    Ok((key, ip))
}

async fn status(state: &AppState, user_id: i32) -> ApiResult<MfaStatus> {
    let (enabled_at, required): (Option<DateTime<Utc>>, bool) = sqlx::query_as(
        "SELECT IF(m.secret IS NULL, NULL, m.enabled_at), COALESCE(u.is_superuser, 0) <> 0
           FROM users u LEFT JOIN user_mfa m ON m.user_id = u.id
          WHERE u.id = ?",
    )
    .bind(user_id)
    .fetch_one(&state.pool)
    .await?;
    Ok(MfaStatus {
        enabled: enabled_at.is_some(),
        enabled_at,
        required,
    })
}

/// My two-step sign-in
///
/// Whether two-step sign-in (an authenticator app code after the password)
/// is on for the caller, since when, and whether they must keep it on
/// (superusers). Turn it on with `POST /v1/me/mfa/setup` then `POST
/// /v1/me/mfa/confirm`; off with `POST /v1/me/mfa/disable`.
#[crate::openapi::path(
    get,
    path = "/v1/me/mfa",
    operation_id = "get_my_mfa",
    tag = "me",
    responses(
        (status = 200, description = "My two-step sign-in", body = MfaStatus),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []), ("device_token" = []))
)]
pub async fn get_mfa(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<Json<MfaStatus>> {
    Ok(Json(status(&state, user.id).await?))
}

/// Start setting up two-step sign-in
///
/// A new secret for an authenticator app (Google Authenticator, Microsoft
/// Authenticator, 1Password...): a QR code to scan, an `otpauth://` link to
/// open on the phone that has the app, and the secret to type in. Asking
/// again within 10 minutes answers the same secret. Nothing changes until
/// `POST /v1/me/mfa/confirm` gets a code from it; when it is already on,
/// confirming replaces the old authenticator.
#[crate::openapi::path(
    post,
    path = "/v1/me/mfa/setup",
    operation_id = "start_my_mfa_setup",
    tag = "me",
    responses(
        (status = 200, description = "What to add to the authenticator app", body = AuthenticatorSetup),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []), ("device_token" = []))
)]
pub async fn start_setup(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<Response> {
    let setup = mfa::start_setup(&state, user.id, &user.email).await?;
    Ok(([(CACHE_CONTROL, "no-store")], Json(setup)).into_response())
}

/// Turn on two-step sign-in
///
/// The code the authenticator app shows for the secret of `POST
/// /v1/me/mfa/setup`: turns two-step sign-in on (from the next sign-in,
/// every sign-in asks for a code; sessions signed in now stay). 400 when
/// the code is wrong or no setup was started in the last hour.
#[crate::openapi::path(
    post,
    path = "/v1/me/mfa/confirm",
    operation_id = "confirm_my_mfa",
    tag = "me",
    request_body = MfaCode,
    responses(
        (status = 200, description = "On", body = MfaStatus),
        (status = 400, description = "Wrong code, or no setup started", body = ErrorBody),
        (status = 429, description = "Too many wrong codes; try later", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []), ("device_token" = []))
)]
pub async fn confirm(
    State(state): State<AppState>,
    headers: HeaderMap,
    CurrentUser(user): CurrentUser,
    JsonBody(input): JsonBody<MfaCode>,
) -> ApiResult<Json<MfaStatus>> {
    let (key, ip) = check_limits(&state, user.id, &headers).await?;
    if !mfa::confirm_setup(&state, user.id, &input.code).await? {
        record_failed_login(&state.pool, &key, &ip).await?;
        return Err(ApiError::BadRequest(WRONG_CODE.into()));
    }
    Ok(Json(status(&state, user.id).await?))
}

/// Turn off two-step sign-in
///
/// Needs a current code of the authenticator app. Superusers must keep it
/// on (403). Sign-ins then ask for the password only.
#[crate::openapi::path(
    post,
    path = "/v1/me/mfa/disable",
    operation_id = "disable_my_mfa",
    tag = "me",
    request_body = MfaCode,
    responses(
        (status = 200, description = "Off", body = MfaStatus),
        (status = 400, description = "Wrong code", body = ErrorBody),
        (status = 429, description = "Too many wrong codes; try later", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Superusers must keep two-step sign-in on", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []), ("device_token" = []))
)]
pub async fn disable(
    State(state): State<AppState>,
    headers: HeaderMap,
    CurrentUser(user): CurrentUser,
    JsonBody(input): JsonBody<MfaCode>,
) -> ApiResult<Json<MfaStatus>> {
    if mfa::state_of(&state.pool, user.id).await?.required {
        return Err(ApiError::Denied(
            "Superusers must keep two-step sign-in on.".into(),
        ));
    }
    let (key, ip) = check_limits(&state, user.id, &headers).await?;
    if !mfa::use_code(&state, user.id, &input.code).await? {
        record_failed_login(&state.pool, &key, &ip).await?;
        return Err(ApiError::BadRequest(WRONG_CODE.into()));
    }
    mfa::disable(&state.pool, user.id).await?;
    Ok(Json(status(&state, user.id).await?))
}
