//! Undo send: whether my sent texts and emails wait 10 seconds with Undo before they go
//! out (on by default), on the website and in the phone app.
//!
//! - `GET /v1/me/undo-send`
//! - `PUT /v1/me/undo-send`

use crate::auth::EmployeeUser;
use crate::error::{ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

/// Whether Undo send is on.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UndoSend {
    /// On: a sent text or email waits 10 seconds with Undo before it goes out. Off: it goes
    /// out at once.
    pub on: bool,
}

/// Get my Undo send
///
/// Whether my sent texts and emails wait 10 seconds with Undo before they go out (on by
/// default), on the website and in the phone app.
#[crate::openapi::path(
    get,
    path = "/v1/me/undo-send",
    operation_id = "get_my_undo_send",
    tag = "me",
    responses(
        (status = 200, description = "Undo send", body = UndoSend),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn undo_send(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<UndoSend>> {
    let on: Option<bool> = sqlx::query_scalar("SELECT undo_send <> 0 FROM users WHERE id = ?")
        .bind(user.id)
        .fetch_optional(&state.pool)
        .await?;
    Ok(Json(UndoSend {
        on: on.unwrap_or(true),
    }))
}

/// Turn my Undo send on or off
///
/// My Account on the website, Settings › General in the phone app. Off: my texts and emails
/// go out the moment I send them, with no Undo.
#[crate::openapi::path(
    put,
    path = "/v1/me/undo-send",
    operation_id = "set_my_undo_send",
    tag = "me",
    request_body = UndoSend,
    responses(
        (status = 204, description = "Saved"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_undo_send(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<UndoSend>,
) -> ApiResult<StatusCode> {
    sqlx::query("UPDATE users SET undo_send = ? WHERE id = ?")
        .bind(input.on)
        .bind(user.id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
