//! The client's support conversations, each a separate request: the list
//! (by subject, like a chat app) and one conversation's messages.

use super::{MESSAGE_COLUMNS, MessageRow, SupportMessage, naive_js};
use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::openapi::{IntoParams, ToSchema};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;

/// Conversations the list returns at most, latest first.
const LIST_LIMIT: i64 = 100;
/// Messages one read of a conversation returns at most.
const MESSAGES_LIMIT: i64 = 500;

/// One of the user's support conversations, as their list shows it.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct MyConversation {
    /// Conversation id (send it as `conversation_id` to write in it).
    pub id: i32,
    /// What it is about: written by the AI from the first message.
    pub subject: String,
    /// `open` (support owes an answer), `waiting` (support answered),
    /// `resolved` (done; writing in it opens it again).
    pub status: String,
    /// The start of the latest message (the team's notes left out).
    pub last_message: Option<String>,
    /// The latest message is support's answer.
    pub last_from_support: bool,
    /// When the latest message was written (ISO 8601, UTC).
    #[serde(serialize_with = "naive_js")]
    #[schema(value_type = String, format = DateTime)]
    pub last_message_at: NaiveDateTime,
    /// When it began.
    #[serde(serialize_with = "naive_js")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: NaiveDateTime,
    /// Support answered since the user last read it.
    pub unread: bool,
}

/// The columns of [`MyConversation`]; `lm` is the latest public message.
const MINE_SELECT: &str = "SELECT c.id, c.subject, c.status, LEFT(lm.body, 160) AS last_message,
       COALESCE(lm.from_support, 0) = 1 AS last_from_support,
       COALESCE(lm.created_at, c.created_at) AS last_message_at, c.created_at,
       (c.last_agent_message_at IS NOT NULL
        AND c.last_agent_message_at > COALESCE(c.client_read_at, '1000-01-01')) AS unread
  FROM support_conversations c
  LEFT JOIN support_messages lm ON lm.id = (
       SELECT MAX(id) FROM support_messages WHERE conversation_id = c.id AND internal = 0)";

/// `GET /v1/support/conversations`.
#[derive(Debug, Serialize, ToSchema)]
pub struct MyConversations {
    /// The user's conversations, latest message first (at most 100).
    pub conversations: Vec<MyConversation>,
    /// One of them is open or waiting: answers may still come.
    pub active: bool,
    /// Conversations with an answer not read yet.
    pub unread: i64,
}

/// List my support conversations
///
/// The signed-in user's conversations with Granite Manager's support team,
/// each a separate request named by its subject (written by the AI from the
/// first message), latest message first, at most 100: status, the start of
/// the latest message, unread answers. Open one with
/// `GET /v1/support/conversations/{id}`; start another with
/// `POST /v1/support/messages` and `new_conversation`.
#[crate::openapi::path(
    get,
    path = "/v1/support/conversations",
    operation_id = "list_my_support_conversations",
    tag = "support",
    responses(
        (status = 200, description = "The conversations", body = MyConversations),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_mine(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<Json<MyConversations>> {
    let conversations: Vec<MyConversation> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{MINE_SELECT} WHERE c.user_id = ?
          ORDER BY last_message_at DESC, c.id DESC LIMIT {LIST_LIMIT}"
    )))
    .bind(user.id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(MyConversations {
        active: conversations.iter().any(|c| c.status != "resolved"),
        unread: i64::try_from(conversations.iter().filter(|c| c.unread).count()).unwrap_or(0),
        conversations,
    }))
}

/// Query of `GET /v1/support/conversations/{id}`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct MineQuery {
    /// Only messages newer than this message id (the newest one the caller
    /// has), for polling.
    pub after: Option<i64>,
}

/// `GET /v1/support/conversations/{id}`.
#[derive(Debug, Serialize, ToSchema)]
pub struct MyConversationDetail {
    /// The conversation.
    pub conversation: MyConversation,
    /// Its messages, oldest first (the team's internal notes left out).
    pub messages: Vec<SupportMessage>,
}

async fn mine(pool: &MySqlPool, id: i32, user_id: i32) -> ApiResult<MyConversation> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{MINE_SELECT} WHERE c.id = ? AND c.user_id = ?"
    )))
    .bind(id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?
    .ok_or(ApiError::NotFound)
}

/// Open one of my support conversations
///
/// One of the signed-in user's conversations with its messages, oldest
/// first (the team's internal notes are never included). With `after` (the
/// newest message id the caller has), only newer ones, so a poll that finds
/// nothing is a tiny answer. Write in it with `POST /v1/support/messages`
/// and its `conversation_id`; mark answers read with
/// `POST /v1/support/conversations/{id}/read`. Another user's conversation
/// is 404.
#[crate::openapi::path(
    get,
    path = "/v1/support/conversations/{id}",
    operation_id = "get_my_support_conversation",
    tag = "support",
    params(("id" = i32, Path, description = "Conversation id"), MineQuery),
    responses(
        (status = 200, description = "The conversation", body = MyConversationDetail),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 404, description = "No such conversation of the user", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_mine(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
    Query(query): Query<MineQuery>,
) -> ApiResult<Json<MyConversationDetail>> {
    let conversation = mine(&state.pool, id, user.id).await?;
    let rows: Vec<MessageRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {MESSAGE_COLUMNS}
           FROM support_messages m
           LEFT JOIN users au ON au.id = m.author_id
          WHERE m.conversation_id = ? AND m.internal = 0 AND m.id > ?
          ORDER BY m.id LIMIT {MESSAGES_LIMIT}"
    )))
    .bind(id)
    .bind(query.after.unwrap_or(0))
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(MyConversationDetail {
        conversation,
        messages: rows.into_iter().map(MessageRow::for_client).collect(),
    }))
}

/// Mark a support conversation read
///
/// Marks support's answers in one of the signed-in user's conversations as
/// read (its unread mark and its share of the Chat button's badge go).
/// Another user's conversation is 404.
#[crate::openapi::path(
    post,
    path = "/v1/support/conversations/{id}/read",
    operation_id = "mark_my_support_conversation_read",
    tag = "support",
    params(("id" = i32, Path, description = "Conversation id")),
    responses(
        (status = 204, description = "Marked read"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 404, description = "No such conversation of the user", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn read_mine(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    let done = sqlx::query(
        "UPDATE support_conversations SET client_read_at = NOW(3) WHERE id = ? AND user_id = ?",
    )
    .bind(id)
    .bind(user.id)
    .execute(&state.pool)
    .await?;
    if done.rows_affected() == 0 {
        // Read again in the same millisecond also changes nothing.
        mine(&state.pool, id, user.id).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}
