//! The client's side of the support chat: their own conversations only.

use super::{MAX_IMAGES, MESSAGE_COLUMNS, MessageRow, SupportMessage, message_body};
use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::{IntoParams, ToSchema};
use crate::routes::uploads::{self, schemas::UploadFolder};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::header::USER_AGENT;
use axum::http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;

/// Messages the first read of the thread returns, newest kept.
const FIRST_PAGE: i64 = 100;
/// Messages one poll returns at most.
const POLL_PAGE: i64 = 200;

/// Query of `GET /v1/support/thread`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ThreadQuery {
    /// Only messages newer than this message id (the newest one the caller
    /// has). Leave out for the latest 100.
    pub after: Option<i64>,
}

/// `GET /v1/support/thread`.
#[derive(Debug, Serialize, ToSchema)]
pub struct SupportThread {
    /// The caller's messages with support and support's answers, oldest
    /// first, over all their requests (internal notes left out).
    pub messages: Vec<SupportMessage>,
    /// A request of theirs is open or waiting on them: poll for answers
    /// while the support chat is on screen. False: nothing to wait for.
    pub active: bool,
    /// The status of their latest request (`open`, `waiting`, `resolved`);
    /// null when they never wrote to support.
    pub status: Option<String>,
    /// Their requests with an answer they have not read.
    pub unread: i64,
}

/// The caller's requests with an answer of support they have not read (the
/// Chat button's badge).
pub async fn client_unread(pool: &MySqlPool, user_id: i32) -> sqlx::Result<i64> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM support_conversations
          WHERE user_id = ? AND last_agent_message_at IS NOT NULL
            AND last_agent_message_at > COALESCE(client_read_at, '1000-01-01')",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
}

/// Get my support chat
///
/// The signed-in user's conversation with Granite Manager's support team:
/// their messages and the team's answers over all their support requests,
/// oldest first (the team's internal notes are never included). Without
/// `after`, the latest 100; with `after` (the newest message id the caller
/// has), only newer ones (at most 200), so a poll that finds nothing is a
/// tiny answer. `active` says whether a request is still open, `unread` how
/// many have answers not yet read (mark them with
/// `POST /v1/support/thread/read`). Write with `POST /v1/support/messages`.
#[crate::openapi::path(
    get,
    path = "/v1/support/thread",
    operation_id = "get_support_thread",
    tag = "support",
    params(ThreadQuery),
    responses(
        (status = 200, description = "The messages and the thread's state", body = SupportThread),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_thread(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(query): Query<ThreadQuery>,
) -> ApiResult<Json<SupportThread>> {
    let pool = &state.pool;
    let latest: Option<String> = sqlx::query_scalar(
        "SELECT status FROM support_conversations WHERE user_id = ? ORDER BY id DESC LIMIT 1",
    )
    .bind(user.id)
    .fetch_optional(pool)
    .await?;
    // Nobody who never wrote has messages: skip the reads.
    let (messages, unread) = if latest.is_none() {
        (Vec::new(), 0)
    } else {
        let rows: Vec<MessageRow> = if let Some(after) = query.after {
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT {MESSAGE_COLUMNS}
                       FROM support_messages m
                       JOIN support_conversations c ON c.id = m.conversation_id
                       LEFT JOIN users au ON au.id = m.author_id
                      WHERE c.user_id = ? AND m.internal = 0 AND m.id > ?
                      ORDER BY m.id LIMIT {POLL_PAGE}"
            )))
            .bind(user.id)
            .bind(after)
            .fetch_all(pool)
            .await?
        } else {
            let mut rows: Vec<MessageRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT {MESSAGE_COLUMNS}
                       FROM support_messages m
                       JOIN support_conversations c ON c.id = m.conversation_id
                       LEFT JOIN users au ON au.id = m.author_id
                      WHERE c.user_id = ? AND m.internal = 0
                      ORDER BY m.id DESC LIMIT {FIRST_PAGE}"
            )))
            .bind(user.id)
            .fetch_all(pool)
            .await?;
            rows.reverse();
            rows
        };
        let unread = client_unread(pool, user.id).await?;
        (
            rows.into_iter().map(MessageRow::for_client).collect(),
            unread,
        )
    };
    Ok(Json(SupportThread {
        messages,
        active: latest.as_deref().is_some_and(|s| s != "resolved"),
        status: latest,
        unread,
    }))
}

/// Body of `POST /v1/support/messages`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewSupportMessage {
    /// The message (at most 5000 characters).
    pub body: String,
    /// Pictures uploaded to the `bug-reports` folder (`POST /v1/uploads`),
    /// at most 4.
    #[serde(default)]
    pub images: Vec<String>,
    /// The CRM page the user is on (path and query).
    #[serde(default)]
    pub page_path: Option<String>,
    /// The user's PostHog session id (`posthog.get_session_id()`), so the
    /// team can watch the recording.
    #[serde(default)]
    pub posthog_session_id: Option<String>,
    /// How many seconds into that session's recording the user wrote.
    #[serde(default)]
    pub replay_seconds: Option<i32>,
    /// The browser window size, `WIDTHxHEIGHT`.
    #[serde(default)]
    pub screen: Option<String>,
}

/// `value` when it looks like a PostHog session id (UUID-like).
fn session_id(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_string()).filter(|v| {
        !v.is_empty() && v.len() <= 64 && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

fn page_path(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().chars().take(500).collect::<String>())
        .filter(|v| v.starts_with('/'))
}

fn screen(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_string()).filter(|v| {
        !v.is_empty() && v.len() <= 32 && v.chars().all(|c| c.is_ascii_digit() || c == 'x')
    })
}

/// The subject of a new request: the first line of its first message.
pub fn subject_of(body: &str) -> String {
    let line = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let mut subject: String = line.chars().take(120).collect();
    if line.chars().count() > 120 {
        subject.push('…');
    }
    subject
}

/// Write to support
///
/// Sends a message to Granite Manager's support team. It joins the user's
/// open request (status `open` or `waiting`); when they have none, it
/// starts a new one whose subject is the message's first line. The request
/// becomes `open` (the team owes an answer). Pictures must be uploaded to
/// the `bug-reports` folder first (`POST /v1/uploads`). The page, the
/// PostHog session and the window size help the team see what the user
/// saw. Returns the saved message; answers come back through
/// `GET /v1/support/thread`.
#[crate::openapi::path(
    post,
    path = "/v1/support/messages",
    operation_id = "send_support_message",
    tag = "support",
    request_body = NewSupportMessage,
    responses(
        (status = 201, description = "Sent", body = SupportMessage),
        (status = 400, description = "Empty or too long, or a picture that was not uploaded for this", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn send_message(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    headers: HeaderMap,
    JsonBody(input): JsonBody<NewSupportMessage>,
) -> ApiResult<(StatusCode, Json<SupportMessage>)> {
    let body = message_body(&input.body)?;
    let images: Vec<String> = input
        .images
        .into_iter()
        .map(|url| url.trim().to_string())
        .filter(|url| !url.is_empty())
        .collect();
    if images.len() > MAX_IMAGES {
        return Err(ApiError::BadRequest(format!(
            "Add at most {MAX_IMAGES} pictures to a message."
        )));
    }
    if images.iter().any(|url| {
        uploads::accepted_key(
            &state.storage,
            url,
            UploadFolder::BugReports,
            user.company_id,
        )
        .is_none()
    }) {
        return Err(ApiError::BadRequest(
            "Add pictures uploaded for support.".into(),
        ));
    }
    let user_agent: Option<String> = headers
        .get(USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.chars().take(300).collect());
    let posthog_session = session_id(input.posthog_session_id);
    let replay_seconds = posthog_session
        .as_ref()
        .and(input.replay_seconds)
        .filter(|s| *s >= 0);

    let mut tx = state.pool.begin().await?;
    let open: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM support_conversations
          WHERE user_id = ? AND status IN ('open', 'waiting')
          ORDER BY id DESC LIMIT 1 FOR UPDATE",
    )
    .bind(user.id)
    .fetch_optional(&mut *tx)
    .await?;
    let conversation_id = if let Some(id) = open {
        id
    } else {
        let id = sqlx::query(
            "INSERT INTO support_conversations (company_id, user_id, subject) VALUES (?, ?, ?)",
        )
        .bind(user.company_id)
        .bind(user.id)
        .bind(subject_of(&body))
        .execute(&mut *tx)
        .await?
        .last_insert_id();
        i32::try_from(id).map_err(|err| ApiError::Internal(err.to_string()))?
    };
    let message_id = sqlx::query(
        "INSERT INTO support_messages
            (conversation_id, author_id, from_support, internal, body, image_urls, page_path,
             posthog_session_id, replay_seconds, user_agent, screen)
         VALUES (?, ?, 0, 0, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(conversation_id)
    .bind(user.id)
    .bind(&body)
    .bind((!images.is_empty()).then(|| serde_json::to_string(&images).unwrap_or_default()))
    .bind(page_path(input.page_path))
    .bind(posthog_session)
    .bind(replay_seconds)
    .bind(user_agent)
    .bind(screen(input.screen))
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    let message_id =
        i64::try_from(message_id).map_err(|err| ApiError::Internal(err.to_string()))?;
    sqlx::query(
        "UPDATE support_conversations
            SET status = 'open', last_message_id = ?, last_message_at = NOW(3),
                last_client_message_at = NOW(3), client_read_at = NOW(3)
          WHERE id = ?",
    )
    .bind(message_id)
    .bind(conversation_id)
    .execute(&mut *tx)
    .await?;
    let row: MessageRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {MESSAGE_COLUMNS} FROM support_messages m
           LEFT JOIN users au ON au.id = m.author_id WHERE m.id = ?"
    )))
    .bind(message_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(row.for_client())))
}

/// Mark support's answers read
///
/// Marks every answer of the support team to the signed-in user as read,
/// which clears the Chat button's support badge (`support_unread` of
/// `GET /v1/me/badges`, `unread` of `GET /v1/support/thread`). Call it when
/// the support chat shows new answers.
#[crate::openapi::path(
    post,
    path = "/v1/support/thread/read",
    operation_id = "mark_support_thread_read",
    tag = "support",
    responses(
        (status = 204, description = "Marked read"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn mark_thread_read(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<StatusCode> {
    sqlx::query(
        "UPDATE support_conversations SET client_read_at = NOW(3)
          WHERE user_id = ? AND last_agent_message_at IS NOT NULL
            AND last_agent_message_at > COALESCE(client_read_at, '1000-01-01')",
    )
    .bind(user.id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subjects_are_the_first_line() {
        assert_eq!(
            subject_of("\n  Can't save a deal \nmore"),
            "Can't save a deal"
        );
        assert_eq!(subject_of(&"a".repeat(130)).chars().count(), 121);
    }

    #[test]
    fn only_safe_session_ids_and_screens() {
        assert_eq!(
            session_id(Some("0192f-ab12".into())).as_deref(),
            Some("0192f-ab12")
        );
        assert_eq!(session_id(Some("x/../y".into())), None);
        assert_eq!(screen(Some("1440x900".into())).as_deref(), Some("1440x900"));
        assert_eq!(screen(Some("<b>".into())), None);
        assert_eq!(page_path(Some("javascript:alert(1)".into())), None);
    }
}
