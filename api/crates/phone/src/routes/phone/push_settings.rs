//! `GET|PUT /v1/phone/push-settings`: which pushes my phones get
//! (Settings › Notifications in the app).

use crate::auth::device::DeviceUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::phone::push_mutes;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};

/// One push switch.
#[derive(Debug, Serialize, ToSchema)]
pub struct PushSetting {
    /// What the push is (`missed_call`, `text`, `lead_new`, `task_due`...).
    pub key: &'static str,
    /// The push is sent to my phones.
    pub on: bool,
}

/// `GET /v1/phone/push-settings`.
#[derive(Debug, Serialize, ToSchema)]
pub struct PushSettings {
    /// The switches I get, in order: the lead switches only sales managers
    /// get are left out for others, as are maintenance (the shop's) and
    /// promise reminders (only for those who have them). The 911 alert has
    /// none: it always goes out.
    pub settings: Vec<PushSetting>,
}

/// Body of `PUT /v1/phone/push-settings`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PushSettingInput {
    /// A key from `GET /v1/phone/push-settings`.
    pub key: String,
    pub on: bool,
}

async fn load(state: &AppState, user_id: i32, company_id: i32) -> ApiResult<PushSettings> {
    let available = push_mutes::available(&state.pool, user_id, company_id).await?;
    let muted = push_mutes::muted(&state.pool, user_id).await?;
    Ok(PushSettings {
        settings: available
            .into_iter()
            .map(|key| PushSetting {
                key,
                on: !muted.iter().any(|m| m == key),
            })
            .collect(),
    })
}

/// List my push switches
///
/// Every kind of push I can turn off, and whether it is on; all are on
/// until turned off. Device token only.
#[crate::openapi::path(
    get,
    path = "/v1/phone/push-settings",
    operation_id = "get_phone_push_settings",
    tag = "phone",
    responses(
        (status = 200, description = "My push switches", body = PushSettings),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn get_push_settings(
    State(state): State<AppState>,
    device: DeviceUser,
) -> ApiResult<Json<PushSettings>> {
    let user = &device.user;
    Ok(Json(load(&state, user.id, user.company_id).await?))
}

/// Turn a push on or off
///
/// Saved for me, so it holds on every phone I sign in on; the bell still
/// lists what a muted push would have said. Returns all my switches.
/// Device token only.
#[crate::openapi::path(
    put,
    path = "/v1/phone/push-settings",
    operation_id = "set_phone_push_setting",
    tag = "phone",
    request_body = PushSettingInput,
    responses(
        (status = 200, description = "My push switches", body = PushSettings),
        (status = 400, description = "Not one of my switches", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn set_push_setting(
    State(state): State<AppState>,
    device: DeviceUser,
    JsonBody(input): JsonBody<PushSettingInput>,
) -> ApiResult<Json<PushSettings>> {
    let user = &device.user;
    let available = push_mutes::available(&state.pool, user.id, user.company_id).await?;
    let Some(key) = available.into_iter().find(|key| *key == input.key) else {
        return Err(ApiError::BadRequest(format!(
            "{} is not one of your notification switches",
            input.key
        )));
    };
    push_mutes::set(&state.pool, user.id, key, input.on).await?;
    Ok(Json(load(&state, user.id, user.company_id).await?))
}
