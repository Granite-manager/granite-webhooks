use super::queries::{
    available_slab_in_company, delete_slab, hold_slab, release_slab, slab_in_company, slabs_by_ids,
    update_slab,
};
use super::schemas::{HoldSlab, ListSlabsQuery, SlabSummary, UpdateSlab};
use crate::auth::{AdminUser, EmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::{JsonBody, MultiQuery};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;

/// Change a slab's size or number
///
/// Sets the length and width (inches, both positive, sent together) and/or
/// the bundle label of one slab or remnant of the caller's company. Returns
/// no body; reload the stone's slabs (`GET /v1/stones/{id}/slabs`) to see it.
#[crate::openapi::path(
    patch,
    path = "/v1/slabs/{id}",
    operation_id = "update_slab",
    tag = "slabs",
    params(("id" = i64, Path, description = "Slab id")),
    request_body = UpdateSlab,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Nothing to change, only one of length/width, a non-positive size, or an empty bundle", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such slab in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
    JsonBody(body): JsonBody<UpdateSlab>,
) -> ApiResult<StatusCode> {
    let size = match (body.length, body.width) {
        (None, None) => None,
        (Some(length), Some(width)) => {
            for (name, value) in [("length", length), ("width", width)] {
                if !value.is_finite() || value <= 0.0 {
                    return Err(ApiError::BadRequest(format!(
                        "{name} must be a positive number"
                    )));
                }
            }
            Some((length, width))
        }
        _ => {
            return Err(ApiError::BadRequest(
                "length and width must be sent together".into(),
            ));
        }
    };
    let bundle = match body.bundle.as_deref().map(str::trim) {
        Some("") => return Err(ApiError::BadRequest("Bundle name is required".into())),
        other => other,
    };
    if size.is_none() && bundle.is_none() {
        return Err(ApiError::BadRequest("nothing to change".into()));
    }
    if !slab_in_company(&state.pool, user.company_id, id).await? {
        return Err(ApiError::NotFound);
    }
    update_slab(&state.pool, id, size, bundle).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a slab
///
/// Removes a slab of the caller's company from the yard (soft delete: it
/// disappears from every list).
#[crate::openapi::path(
    delete,
    path = "/v1/slabs/{id}",
    operation_id = "delete_slab",
    tag = "slabs",
    params(("id" = i64, Path, description = "Slab id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such slab in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    if !slab_in_company(&state.pool, user.company_id, id).await? {
        return Err(ApiError::NotFound);
    }
    delete_slab(&state.pool, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Get slabs by id
///
/// Returns the slabs of the caller's company among the given ids (others are
/// left out), each with its bundle label and its stone (id, name, type).
/// Use it to label slabs already chosen on a sale.
#[crate::openapi::path(
    get,
    path = "/v1/slabs",
    operation_id = "list_slabs_by_id",
    tag = "slabs",
    params(ListSlabsQuery),
    responses(
        (status = 200, description = "Slabs by id", body = Vec<SlabSummary>),
        (status = 400, description = "An id is not a number", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<ListSlabsQuery>,
) -> ApiResult<Json<Vec<SlabSummary>>> {
    Ok(Json(
        slabs_by_ids(&state.pool, user.company_id, &query.ids).await?,
    ))
}

/// Hold lengths a slab can be put on hold for, in days.
pub const HOLD_DAYS: [i32; 5] = [3, 5, 7, 12, 15];

/// Put a slab on hold
///
/// Holds an available (unsold, uncut) slab or remnant of the caller's
/// company for 3, 5, 7, 12 or 15 days from now; the hold ends by itself.
/// Holding again replaces the old hold. `GET /v1/stones/{id}/slabs` shows it
/// as `hold`.
#[crate::openapi::path(
    put,
    path = "/v1/slabs/{id}/hold",
    operation_id = "hold_slab",
    tag = "slabs",
    params(("id" = i64, Path, description = "Slab id")),
    request_body = HoldSlab,
    responses(
        (status = 204, description = "On hold"),
        (status = 400, description = "Days is not 3, 5, 7, 12 or 15", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such available slab in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn hold(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
    JsonBody(body): JsonBody<HoldSlab>,
) -> ApiResult<StatusCode> {
    if !HOLD_DAYS.contains(&body.days) {
        return Err(ApiError::BadRequest(
            "days must be 3, 5, 7, 12 or 15".into(),
        ));
    }
    if !available_slab_in_company(&state.pool, user.company_id, id).await? {
        return Err(ApiError::NotFound);
    }
    hold_slab(&state.pool, id, user.id, body.days).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Take a slab off hold
///
/// Ends the hold of a slab of the caller's company now. A slab that is not
/// on hold stays as it is.
#[crate::openapi::path(
    delete,
    path = "/v1/slabs/{id}/hold",
    operation_id = "release_slab_hold",
    tag = "slabs",
    params(("id" = i64, Path, description = "Slab id")),
    responses(
        (status = 204, description = "Off hold"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such slab in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn release(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    if !slab_in_company(&state.pool, user.company_id, id).await? {
        return Err(ApiError::NotFound);
    }
    release_slab(&state.pool, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
