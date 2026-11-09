//! Where addresses are, for the schedule's events map.
//!
//! - `POST /v1/schedule/geocode` → the place on the map of each address
//!   (Events map skill)

use crate::auth::ShopOrEmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::maps::Point;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};

/// Most addresses placed in one ask; the app sends longer lists in parts.
pub const MAX_ADDRESSES: usize = 50;

/// Body of `POST /v1/schedule/geocode`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct GeocodeInput {
    /// The addresses, at most 50.
    pub addresses: Vec<String>,
}

/// `POST /v1/schedule/geocode`.
#[derive(Debug, Serialize, ToSchema)]
pub struct Geocoded {
    /// Where each address is, in the order asked; null where Google cannot
    /// find it or Google Maps is not set up.
    pub points: Vec<Option<Point>>,
}

/// Place addresses on the map
///
/// Where each address is, for the pins of the schedule's events map.
#[crate::openapi::path(
    post,
    path = "/v1/schedule/geocode",
    operation_id = "schedule_geocode",
    tag = "schedule",
    request_body = GeocodeInput,
    responses(
        (status = 200, description = "The places", body = Geocoded),
        (status = 400, description = "More than 50 addresses", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller lacks the Events map skill", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn geocode(
    State(state): State<AppState>,
    ShopOrEmployeeUser(_user): ShopOrEmployeeUser,
    JsonBody(input): JsonBody<GeocodeInput>,
) -> ApiResult<Json<Geocoded>> {
    if input.addresses.len() > MAX_ADDRESSES {
        return Err(ApiError::BadRequest(format!(
            "At most {MAX_ADDRESSES} addresses can be placed at once."
        )));
    }
    let points = state.maps.geocode_all(&input.addresses).await;
    Ok(Json(Geocoded { points }))
}
