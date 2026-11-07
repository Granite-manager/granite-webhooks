//! Deleting a text that failed: `DELETE /v1/sms/{provider}/messages/{id}`.
//!
//! Only the rep who sent it, and only while it is still failed: a text that
//! went out, or one a retry is sending now, stays. Its files go with it.

use super::provider::SmsProvider;
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;

/// Delete a failed text
///
/// Removes one of the caller's own failed outbound texts (`provider` names
/// where it is stored) from the conversation, with its files.
#[crate::openapi::path(
    delete,
    path = "/v1/sms/{provider}/messages/{id}",
    operation_id = "delete_failed_sms",
    tag = "sms",
    params(
        ("provider" = SmsProvider, Path, description = "SMS provider"),
        ("id" = i32, Path, description = "The failed message"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 404, description = "Not a failed text of the caller's", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_failed(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((provider, id)): Path<(SmsProvider, i32)>,
) -> ApiResult<StatusCode> {
    let done = sqlx::query(sqlx::AssertSqlSafe(format!(
        "DELETE FROM {} WHERE id = ? AND company_id = ? AND sender_user_id = ?
            AND direction = 'outbound' AND status = 'failed'",
        provider.sms_table()
    )))
    .bind(id)
    .bind(user.company_id)
    .bind(user.id)
    .execute(&state.pool)
    .await?;
    if done.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}
