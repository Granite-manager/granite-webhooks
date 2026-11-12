//! New `RingCentral` and `CloudTalk` texts on the phones, as Telnyx texts
//! are: `POST /v1/sms/inbound-push` (the `webhooks` Lambda, once it stored
//! the text) pushes `{granite: "1", type: "crm_text", provider, smsId}` to
//! the signed-in phones of the users whose own inbox (`/v1/sms/mine`) shows
//! the conversation; the app reads who wrote and what with
//! `GET /v1/sms/mine/messages/{provider}/{id}` and opens the conversation.

use super::provider::SmsProvider;
use super::scope::{SmsScope, thread_access};
use super::thread::customer_by_phone;
use crate::auth::EmployeeUser;
use crate::auth::job::JobCaller;
use crate::auth::session::find_user;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::phone::devices::{LIVE_DEVICE, push_to_users};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;
use std::collections::BTreeMap;

/// Body of `POST /v1/sms/inbound-push`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct InboundTextPush {
    /// The company the text came to.
    pub company_id: i32,
    /// `ringcentral` or `cloudtalk` (Telnyx texts push on their own).
    pub provider: String,
    /// The stored row of the provider's SMS table.
    pub sms_id: i64,
}

/// How many phones were told.
#[derive(Debug, Serialize, ToSchema)]
pub struct InboundTextPushed {
    /// Phones FCM accepted the push for; 0 when nobody's inbox shows it.
    pub pushed: usize,
}

/// A new text as its notification shows it.
#[derive(Debug, Serialize, ToSchema)]
pub struct TextNotice {
    /// The other party's phone (last ten digits): the conversation is
    /// `GET /v1/sms/mine/threads/{phone}`.
    pub phone: String,
    /// The customer the phone belongs to; null when none.
    pub customer_name: Option<String>,
    /// What they wrote; null for a picture or file only.
    pub text: Option<String>,
    /// It came with pictures or files.
    pub has_media: bool,
}

/// An inbound text of the company: its conversation's phone and its text.
async fn inbound_text(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
    id: i64,
) -> sqlx::Result<Option<(String, Option<String>)>> {
    let table = provider.sms_table();
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT phone_digits, text FROM {table}
          WHERE id = ? AND company_id = ? AND direction = 'inbound' AND is_echo = 0
            AND phone_digits IS NOT NULL AND phone_digits <> ''"
    )))
    .bind(id)
    .bind(company_id)
    .fetch_optional(pool)
    .await
}

/// `ringcentral` or `cloudtalk`; Telnyx has its own push.
fn crm_provider(name: &str) -> Option<SmsProvider> {
    SmsProvider::parse(name).filter(|provider| *provider != SmsProvider::Telnyx)
}

/// Push a new `RingCentral` or `CloudTalk` text to phones
///
/// Called by the `webhooks` Lambda (job key) once it stored an inbound
/// text (and its pictures). Pushed as data `{granite: "1", type:
/// "crm_text", provider, smsId}` (ids only: never the number, the name or
/// the text) to the signed-in phones of the company's users whose own
/// inbox shows the conversation, as `GET /v1/sms/mine/threads` lists it.
/// The app then reads `GET /v1/sms/mine/messages/{provider}/{id}`. Nothing
/// is pushed (`pushed: 0`) for an unknown or outbound text or when nobody
/// has it. Calling it again pushes again.
#[crate::openapi::path(
    post,
    path = "/v1/sms/inbound-push",
    operation_id = "push_inbound_sms",
    tag = "sms",
    request_body = InboundTextPush,
    responses(
        (status = 200, description = "Phones told", body = InboundTextPushed),
        (status = 400, description = "Invalid body or provider", body = ErrorBody),
        (status = 401, description = "Missing or wrong key", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("job_key" = []))
)]
pub async fn inbound_push(
    State(state): State<AppState>,
    _caller: JobCaller,
    JsonBody(body): JsonBody<InboundTextPush>,
) -> ApiResult<Json<InboundTextPushed>> {
    let provider = crm_provider(&body.provider)
        .ok_or_else(|| ApiError::BadRequest("provider must be ringcentral or cloudtalk".into()))?;
    let nobody = Json(InboundTextPushed { pushed: 0 });
    let Some((phone, _)) =
        inbound_text(&state.pool, provider, body.company_id, body.sms_id).await?
    else {
        return Ok(nobody);
    };
    // Only users with a signed-in phone can be told.
    let candidates: Vec<i32> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT DISTINCT pd.user_id FROM phone_devices pd
           JOIN device_sessions ds ON ds.id = pd.device_session_id
          WHERE pd.push_token IS NOT NULL AND {LIVE_DEVICE} AND pd.company_id = ?
          ORDER BY pd.user_id"
    )))
    .bind(body.company_id)
    .fetch_all(&state.pool)
    .await?;
    let mut users = Vec::new();
    for user_id in candidates {
        let Some(user) = find_user(&state.pool, user_id, body.company_id).await? else {
            continue;
        };
        if thread_access(&state.pool, provider, &user, SmsScope::Mine, None, &phone)
            .await?
            .is_some()
        {
            users.push(user_id);
        }
    }
    let data = BTreeMap::from([
        ("granite".to_string(), "1".to_string()),
        ("type".to_string(), "crm_text".to_string()),
        ("provider".to_string(), provider.as_str().to_string()),
        ("smsId".to_string(), body.sms_id.to_string()),
    ]);
    let pushed = push_to_users(&state, body.company_id, &users, &data).await?;
    Ok(Json(InboundTextPushed { pushed }))
}

/// Read a new text for its notification
///
/// Who wrote the inbound `RingCentral` or `CloudTalk` text `id` and what,
/// for the phone's notification after a `crm_text` push: the conversation's
/// phone (open it with `GET /v1/sms/mine/threads/{phone}`), the customer's
/// name and the text. 404 unless the text is in the caller's own inbox.
#[crate::openapi::path(
    get,
    path = "/v1/sms/mine/messages/{provider}/{id}",
    operation_id = "get_my_sms_notice",
    tag = "sms",
    params(
        ("provider" = String, Path, description = "`ringcentral` or `cloudtalk`"),
        ("id" = i64, Path, description = "The text's id from the push"),
    ),
    responses(
        (status = 200, description = "The text", body = TextNotice),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "Not a text of the caller's inbox", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn text_notice(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((provider, id)): Path<(String, i64)>,
) -> ApiResult<Json<TextNotice>> {
    let provider = crm_provider(&provider).ok_or(ApiError::NotFound)?;
    let (phone, text) = inbound_text(&state.pool, provider, user.company_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if thread_access(&state.pool, provider, &user, SmsScope::Mine, None, &phone)
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound);
    }
    let (files, column) = provider.attachments();
    let has_media: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT EXISTS (SELECT 1 FROM {files} WHERE {column} = ?)"
    )))
    .bind(id)
    .fetch_one(&state.pool)
    .await?;
    let customer = customer_by_phone(&state.pool, provider, user.company_id, &phone).await?;
    Ok(Json(TextNotice {
        phone,
        customer_name: customer
            .and_then(|c| c.name)
            .filter(|name| !name.trim().is_empty()),
        text: text
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty()),
        has_media,
    }))
}
