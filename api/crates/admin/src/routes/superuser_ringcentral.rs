//! Superusers only: store the `RingCentral` texts the CRM never got.
//!
//! A push the webhook refuses is never sent again, though `RingCentral`
//! keeps the text (2026-11-07, 00:22 to 20:53 UTC: the subscriptions still
//! carried a token the webhook no longer took). This reads each extension's
//! message store for a window and hands every text the CRM does not have to
//! the webhooks Lambda's `/ringcentral/notify/{company_id}`, as its push
//! would have: stored once (`INSERT IGNORE` on the `RingCentral` id), with
//! its pictures, the SMS follow-ups of a customer who replied stopped and
//! the deal moved, like a live text.

use crate::auth::CurrentUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use chrono::{DateTime, Duration, Utc};
use common::telephony::ringcentral::{self, Body};
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::time::Instant;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(catch_up_texts))
}

/// How far back a window may start.
const MAX_DAYS: i64 = 30;
/// Texts per message-store page.
const PER_PAGE: u32 = 250;
/// Pages read per extension at most (5,000 texts).
const MAX_PAGES: u32 = 20;
/// A call starts no new extension after this, well within the api's 30 s.
const BUDGET: std::time::Duration = std::time::Duration::from_secs(15);
/// How long handing one text to the webhook may take.
const HANDOVER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// `RingCentral` ids looked up per query.
const ID_CHUNK: usize = 500;

/// Body of the `RingCentral` texts catch-up: the window and the extensions to read.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CatchUpRequest {
    /// Start of the window, RFC 3339 (`2026-11-07T00:15:00Z`), at most 30
    /// days back.
    pub since: String,
    /// End of the window, RFC 3339; now when left out.
    pub until: Option<String>,
    /// The extensions to read. Left out or empty, nothing is read and
    /// `remaining` lists every extension of the company: its enabled users,
    /// then each rep's own sign-in.
    #[serde(default)]
    pub extensions: Vec<String>,
}

/// What a catch-up call read, and what is left for the next call.
#[derive(Debug, Serialize, ToSchema)]
pub struct CatchUpReport {
    /// The extensions read in this call.
    pub done: Vec<ExtensionReport>,
    /// Extensions for a next call: all of them when none were sent, else
    /// the ones this call had no time for.
    pub remaining: Vec<String>,
}

/// The texts of one extension in the window.
#[derive(Debug, Serialize, ToSchema)]
pub struct ExtensionReport {
    /// The `RingCentral` extension id.
    pub extension: String,
    /// Texts `RingCentral` keeps for the window.
    pub texts: usize,
    /// Of those, the ones the CRM already had.
    pub known: usize,
    /// The others, handed to the webhook to store.
    pub handed_over: usize,
    /// Hand-overs the webhook refused or that failed (logged).
    pub failed: usize,
    /// Why the extension's texts could not be read, when they could not.
    pub error: Option<String>,
}

/// Store missed `RingCentral` texts
///
/// Reads the message store of `extensions` for the window and hands every
/// text the CRM does not have to the webhook that stores pushes. Without
/// `extensions` it only lists them in `remaining`; send one at a time.
#[crate::openapi::path(
    post,
    path = "/v1/superuser/companies/{id}/ringcentral/catch-up",
    operation_id = "catch_up_ringcentral_texts",
    tag = "superuser",
    params(("id" = i32, Path, description = "Company id")),
    request_body = CatchUpRequest,
    responses(
        (status = 200, description = "What was read and handed over", body = CatchUpReport),
        (status = 400, description = "Bad window, or pushes are off on this server", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn catch_up_texts(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(company_id): Path<i32>,
    JsonBody(input): JsonBody<CatchUpRequest>,
) -> ApiResult<Json<CatchUpReport>> {
    superuser(&user)?;
    let Some(target) = state.ringcentral_subscriptions.as_ref() else {
        return Err(ApiError::BadRequest(
            "RingCentral pushes are off on this server".into(),
        ));
    };
    let now = Utc::now();
    let since = parse_time(&input.since, "since")?;
    let until = match input.until.as_deref() {
        Some(until) => parse_time(until, "until")?,
        None => now,
    };
    if since >= until || since < now - Duration::days(MAX_DAYS) {
        return Err(ApiError::BadRequest(format!(
            "since must be before until and at most {MAX_DAYS} days back"
        )));
    }
    let wanted = digits_only(input.extensions);
    if wanted.is_empty() {
        let remaining = company_extensions(&state, company_id).await?;
        return Ok(Json(CatchUpReport {
            done: Vec::new(),
            remaining,
        }));
    }
    // Carries the webhook's token: never logged.
    let address = target.delivery_address(company_id);
    let started = Instant::now();
    let mut done = Vec::new();
    let mut remaining = Vec::new();
    for extension in wanted {
        if !done.is_empty() && started.elapsed() >= BUDGET {
            remaining.push(extension);
            continue;
        }
        let report =
            catch_up_extension(&state, &address, company_id, &extension, since, until).await?;
        tracing::info!(
            company_id,
            extension = %report.extension,
            texts = report.texts,
            known = report.known,
            handed_over = report.handed_over,
            failed = report.failed,
            error = ?report.error,
            "RingCentral texts caught up"
        );
        done.push(report);
    }
    Ok(Json(CatchUpReport { done, remaining }))
}

const fn superuser(user: &SessionUser) -> ApiResult<()> {
    if user.is_superuser {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

fn parse_time(value: &str, field: &str) -> ApiResult<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value.trim())
        .map(|time| time.with_timezone(&Utc))
        .map_err(|_| {
            ApiError::BadRequest(format!(
                "{field} must be an RFC 3339 time, e.g. 2026-11-07T00:15:00Z"
            ))
        })
}

/// Extension ids as `RingCentral` writes them (digits only), each once, in
/// order.
fn digits_only(extensions: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    extensions
        .into_iter()
        .map(|extension| extension.trim().to_string())
        .filter(|extension| {
            !extension.is_empty() && extension.bytes().all(|byte| byte.is_ascii_digit())
        })
        .filter(|extension| seen.insert(extension.clone()))
        .collect()
}

/// The company's enabled users on `RingCentral`, then each rep's own
/// sign-in (whose texts the company's login may not read).
async fn company_extensions(state: &AppState, company_id: i32) -> ApiResult<Vec<String>> {
    let mut extensions = Vec::new();
    match ringcentral::request(
        &state.pool,
        &state.telephony_http,
        company_id,
        reqwest::Method::GET,
        "account/~/extension?type=User&status=Enabled&perPage=1000",
        &Body::Empty,
        None,
    )
    .await
    {
        Ok(found) => extensions.extend(record_ids(&found).into_iter().map(|id| id.to_string())),
        Err(error) => tracing::warn!(
            company_id,
            status = error.status,
            "RingCentral catch-up: could not list the extensions"
        ),
    }
    let reps: Vec<String> =
        sqlx::query_scalar("SELECT extension_id FROM ringcentral_user_logins WHERE company_id = ?")
            .bind(company_id)
            .fetch_all(&state.pool)
            .await?;
    extensions.extend(reps);
    Ok(digits_only(extensions))
}

/// Hands the extension's texts of the window that the CRM lacks to the
/// webhook at `address`.
async fn catch_up_extension(
    state: &AppState,
    address: &str,
    company_id: i32,
    extension: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> ApiResult<ExtensionReport> {
    let mut report = ExtensionReport {
        extension: extension.to_string(),
        texts: 0,
        known: 0,
        handed_over: 0,
        failed: 0,
        error: None,
    };
    let texts = match message_store(state, company_id, extension, since, until).await {
        Ok(texts) => texts,
        Err(error) => {
            report.error = Some(format!("RingCentral answered {}", error.status));
            return Ok(report);
        }
    };
    report.texts = texts.len();
    let ids: Vec<i64> = texts.iter().filter_map(record_id).collect();
    let known = known_ids(&state.pool, company_id, &ids).await?;
    let event = format!("/restapi/v1.0/account/~/extension/{extension}/message-store");
    for text in texts {
        let Some(id) = record_id(&text) else {
            continue;
        };
        if known.contains(&id) {
            report.known += 1;
            continue;
        }
        let push = push_body(&event, extension, id, text);
        match state
            .telephony_http
            .post(address)
            .timeout(HANDOVER_TIMEOUT)
            .json(&push)
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => report.handed_over += 1,
            Ok(response) => {
                report.failed += 1;
                tracing::warn!(
                    company_id,
                    extension,
                    status = response.status().as_u16(),
                    "RingCentral catch-up: the webhook refused a text"
                );
            }
            Err(error) => {
                report.failed += 1;
                tracing::warn!(
                    company_id,
                    extension,
                    error = %error.without_url(),
                    "RingCentral catch-up: the webhook could not be reached"
                );
            }
        }
    }
    Ok(report)
}

/// The extension's texts (both directions) in the window, every page.
async fn message_store(
    state: &AppState,
    company_id: i32,
    extension: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<Value>, ringcentral::ApiError> {
    let mut texts = Vec::new();
    for page in 1..=MAX_PAGES {
        let path = format!(
            "account/~/extension/{extension}/message-store?messageType=SMS&dateFrom={}&dateTo={}&perPage={PER_PAGE}&page={page}",
            ringcentral_time(since),
            ringcentral_time(until),
        );
        let found = ringcentral::request(
            &state.pool,
            &state.telephony_http,
            company_id,
            reqwest::Method::GET,
            &path,
            &Body::Empty,
            None,
        )
        .await?;
        texts.extend(records(&found).iter().cloned());
        if !found["navigation"]["nextPage"].is_object() {
            break;
        }
    }
    Ok(texts)
}

/// Which of `ids` the company already stored.
async fn known_ids(
    pool: &sqlx::MySqlPool,
    company_id: i32,
    ids: &[i64],
) -> ApiResult<HashSet<i64>> {
    let mut known = HashSet::new();
    for chunk in ids.chunks(ID_CHUNK) {
        let sql = format!(
            "SELECT ringcentral_id FROM ringcentral_sms
              WHERE company_id = ? AND ringcentral_id IN ({})",
            vec!["?"; chunk.len()].join(", ")
        );
        let mut query = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql)).bind(company_id);
        for id in chunk {
            query = query.bind(*id);
        }
        known.extend(query.fetch_all(pool).await?);
    }
    Ok(known)
}

/// A message-store record as its push would carry it: the webhook reads
/// `event` (whose extension it is) and `body` (the text).
fn push_body(event: &str, extension: &str, id: i64, text: Value) -> Value {
    json!({
        "uuid": format!("catch-up-{id}"),
        "event": event,
        "timestamp": Utc::now().to_rfc3339(),
        "subscriptionId": "catch-up",
        "ownerId": extension,
        "body": text,
    })
}

fn records(found: &Value) -> &[Value] {
    found["records"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// The `id` of each record (a number, or a string of digits).
fn record_ids(found: &Value) -> Vec<i64> {
    records(found).iter().filter_map(record_id).collect()
}

fn record_id(record: &Value) -> Option<i64> {
    match &record["id"] {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

/// How `RingCentral`'s `dateFrom` and `dateTo` take a time.
fn ringcentral_time(time: DateTime<Utc>) -> String {
    time.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_are_digits_each_once_in_order() {
        let given = ["540273", " 540273 ", "12a", "", "601"].map(String::from);
        assert_eq!(digits_only(given), vec!["540273", "601"]);
    }

    #[test]
    fn record_ids_read_numbers_and_digit_strings() {
        let found = json!({"records": [
            {"id": 2_200_000_001_i64},
            {"id": "2200000002"},
            {"id": "x"},
            {}
        ]});
        assert_eq!(record_ids(&found), vec![2_200_000_001, 2_200_000_002]);
        assert!(record_ids(&json!({})).is_empty());
    }

    #[test]
    fn the_push_carries_the_record_and_its_extension() {
        let text = json!({"id": 7, "type": "SMS", "direction": "Inbound"});
        let event = "/restapi/v1.0/account/~/extension/540273/message-store";
        let push = push_body(event, "540273", 7, text.clone());
        assert_eq!(push["event"], event);
        assert_eq!(push["body"], text);
        assert_eq!(push["ownerId"], "540273");
        assert_eq!(push["uuid"], "catch-up-7");
    }

    #[test]
    fn times_are_written_as_ringcentral_reads_them() {
        let time = parse_time("2026-11-07T00:15:00Z", "since").expect("an RFC 3339 time");
        assert_eq!(ringcentral_time(time), "2026-11-07T00:15:00.000Z");
        assert!(parse_time("yesterday", "since").is_err());
    }
}
