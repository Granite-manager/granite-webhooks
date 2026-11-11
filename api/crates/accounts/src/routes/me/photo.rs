//! My picture: the photo of me that the phone app and my coworkers see.
//!
//! - `PUT /v1/me/photo`
//! - `DELETE /v1/me/photo`

use crate::auth::EmployeeUser;
use crate::error::{ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::routes::uploads::{self, schemas::UploadFolder};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

/// Body of `PUT /v1/me/photo`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PhotoInput {
    /// The `url` that `POST /v1/uploads` (folder `user-photos`) returned,
    /// after the picture was `PUT` to its `upload_url`.
    pub url: String,
}

/// My picture now.
#[derive(Debug, Serialize, ToSchema)]
pub struct MyPhoto {
    /// Public URL of my picture; null when I have none.
    pub photo_url: Option<String>,
}

/// Set my picture
///
/// Makes an uploaded image my picture: first `POST /v1/uploads` with folder
/// `user-photos`, `PUT` the file to its `upload_url`, then send its `url`
/// here. The picture I had before is deleted from storage. 400 when `url`
/// is not a `user-photos` file of my company, or was never uploaded.
#[crate::openapi::path(
    put,
    path = "/v1/me/photo",
    operation_id = "set_my_photo",
    tag = "me",
    request_body = PhotoInput,
    responses(
        (status = 200, description = "My picture now", body = MyPhoto),
        (status = 400, description = "Not an uploaded `user-photos` file of my company", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database or storage error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_photo(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<PhotoInput>,
) -> ApiResult<Json<MyPhoto>> {
    let url = input.url.trim();
    uploads::attach(
        &state.storage,
        url,
        UploadFolder::UserPhotos,
        user.company_id,
    )
    .await?;
    let before = current(&state, user.id).await?;
    sqlx::query("UPDATE users SET photo_url = ? WHERE id = ?")
        .bind(url)
        .bind(user.id)
        .execute(&state.pool)
        .await?;
    if let Some(before) = before.filter(|before| before != url) {
        uploads::remove(&state.storage, &before, user.company_id).await;
    }
    Ok(Json(MyPhoto {
        photo_url: Some(url.to_owned()),
    }))
}

/// Remove my picture
///
/// I have no picture afterwards (my initials show); the file is deleted
/// from storage. Fine to call when I have none.
#[crate::openapi::path(
    delete,
    path = "/v1/me/photo",
    operation_id = "delete_my_photo",
    tag = "me",
    responses(
        (status = 204, description = "Removed"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_photo(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<StatusCode> {
    let before = current(&state, user.id).await?;
    sqlx::query("UPDATE users SET photo_url = NULL WHERE id = ?")
        .bind(user.id)
        .execute(&state.pool)
        .await?;
    if let Some(before) = before {
        uploads::remove(&state.storage, &before, user.company_id).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The user's picture, if any.
async fn current(state: &AppState, user_id: i32) -> sqlx::Result<Option<String>> {
    let url: Option<Option<String>> =
        sqlx::query_scalar("SELECT photo_url FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(&state.pool)
            .await?;
    Ok(url.flatten().filter(|url| !url.trim().is_empty()))
}
