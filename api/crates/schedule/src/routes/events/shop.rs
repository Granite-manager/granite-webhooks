//! The shop's progress on a job: Cut, then Complete.
//!
//! - `POST /v1/events/{id}/shop-status` → a Shop Manager or Shop Worker marks
//!   a job Cut or Complete, or clears it
//!
//! Shop Managers, Shop Workers and Installers see it on the schedule (the
//! board leaves it out for everyone else). It is separate from the crew's
//! On my way / Completed and stays when the event moves to another day.

use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;

/// `positions.id` of the ones who set it (Shop Manager, Shop Worker) and of
/// the ones who only see it (Installer).
const SHOP_MANAGER: i32 = 3;
const SHOP_WORKER: i32 = 4;
const INSTALLER: i32 = 6;

/// The shop's progress on a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ShopStatus {
    Cut,
    Complete,
}

impl ShopStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Cut => "cut",
            Self::Complete => "complete",
        }
    }
}

/// Body of `POST /v1/events/{id}/shop-status`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ShopStatusInput {
    /// `cut`, `complete`, or null to clear it.
    pub status: Option<ShopStatus>,
}

/// `POST /v1/events/{id}/shop-status`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ShopStatusSaved {
    /// The job's shop status now; null when cleared.
    pub status: Option<ShopStatus>,
}

/// Whether `user_id` holds one of `positions` in the company.
async fn has_position(
    pool: &MySqlPool,
    user_id: i32,
    company_id: i32,
    positions: &[i32],
) -> Result<bool, sqlx::Error> {
    let mut sql = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "SELECT EXISTS(SELECT 1 FROM users_positions WHERE user_id = ",
    );
    sql.push_bind(user_id)
        .push(" AND company_id = ")
        .push_bind(company_id)
        .push(" AND position_id IN (");
    let mut ids = sql.separated(", ");
    for position in positions {
        ids.push_bind(*position);
    }
    sql.push("))");
    sql.build_query_scalar::<bool>().fetch_one(pool).await
}

/// A Shop Manager, Shop Worker or Installer: they see the shop's status.
pub async fn sees_shop_status(
    pool: &MySqlPool,
    user_id: i32,
    company_id: i32,
) -> Result<bool, sqlx::Error> {
    has_position(pool, user_id, company_id, &[SHOP_MANAGER, SHOP_WORKER, INSTALLER]).await
}

/// Mark a job Cut or Complete
///
/// Sets the shop's progress on a job (`cut`, then `complete`), or clears it
/// with null. Shop Managers, Shop Workers and Installers see it on the
/// schedule. For Shop Managers and Shop Workers.
#[crate::openapi::path(
    post,
    path = "/v1/events/{id}/shop-status",
    operation_id = "set_shop_status",
    tag = "schedule",
    params(("id" = i32, Path, description = "Event id")),
    request_body = ShopStatusInput,
    responses(
        (status = 200, description = "Saved", body = ShopStatusSaved),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not a Shop Manager or Shop Worker", body = ErrorBody),
        (status = 404, description = "No such event in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_status(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<ShopStatusInput>,
) -> ApiResult<Json<ShopStatusSaved>> {
    let pool = &state.pool;
    let found: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM events e JOIN users u ON u.id = e.created_user_id
                        WHERE e.id = ? AND u.company_id = ? AND e.deleted_date IS NULL)",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_one(pool)
    .await?;
    if !found {
        return Err(ApiError::NotFound);
    }
    if !has_position(pool, user.id, user.company_id, &[SHOP_MANAGER, SHOP_WORKER]).await? {
        return Err(ApiError::Forbidden);
    }
    let status = input.status.map(ShopStatus::as_str);
    sqlx::query(
        "UPDATE events
            SET shop_status = ?,
                shop_status_at = IF(? IS NULL, NULL, UTC_TIMESTAMP()),
                shop_status_by = IF(? IS NULL, NULL, ?)
          WHERE id = ?",
    )
    .bind(status)
    .bind(status)
    .bind(status)
    .bind(user.id)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(Json(ShopStatusSaved {
        status: input.status,
    }))
}
