//! Admin › Deals › Automatic Activities: rules that add an activity to a deal
//! when it moves from one list to another (`deal_stage_activity_rules`). The
//! moves themselves run them: `common::crud::deal_stage_activities`.

use crate::auth::AdminUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::routes::created::CreatedId;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, MySqlPool};

/// A rule, as the admin page shows it.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct ActivityRule {
    /// Rule id.
    pub id: i32,
    /// The list the deal leaves; null: any list.
    pub from_list_id: Option<i32>,
    /// The list the deal lands on.
    pub to_list_id: i32,
    /// Name of the activity it adds.
    pub activity_name: String,
    /// Due this many `due_unit` after the move.
    pub due_amount: i32,
    /// `hours` or `days`.
    pub due_unit: String,
    /// Off: adds nothing.
    pub is_enabled: bool,
}

/// Body of `POST /v1/deal-stage-activity-rules` and `PUT /v1/deal-stage-activity-rules/{id}`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ActivityRuleInput {
    /// The list the deal leaves; null: any list.
    pub from_list_id: Option<i32>,
    /// The list the deal lands on (same pipeline as `from_list_id`).
    pub to_list_id: i32,
    /// Name of the activity, 1–255 characters.
    pub activity_name: String,
    /// 0–999.
    pub due_amount: i32,
    /// `hours` or `days`.
    pub due_unit: String,
    /// Default on.
    pub is_enabled: Option<bool>,
}

/// Group of a live list the company sees (its own pipelines or the shared one).
async fn list_group(pool: &MySqlPool, company_id: i32, list_id: i32) -> ApiResult<i32> {
    let group: Option<i32> = sqlx::query_scalar(
        "SELECT dl.group_id FROM deals_list dl
           JOIN groups_list g ON g.id = dl.group_id AND g.deleted_at IS NULL
          WHERE dl.id = ? AND dl.deleted_at IS NULL AND g.company_id = ?",
    )
    .bind(list_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    group.ok_or_else(|| ApiError::BadRequest(format!("unknown list {list_id}")))
}

/// Checked input: the name trimmed, the unit known, both lists the company's.
async fn check_input(
    pool: &MySqlPool,
    company_id: i32,
    input: &ActivityRuleInput,
) -> ApiResult<String> {
    let name = input.activity_name.trim();
    if name.is_empty() || name.chars().count() > 255 {
        return Err(ApiError::BadRequest("Enter the activity name".into()));
    }
    if !(0..=999).contains(&input.due_amount) {
        return Err(ApiError::BadRequest("Due in must be 0 to 999".into()));
    }
    if input.due_unit != "hours" && input.due_unit != "days" {
        return Err(ApiError::BadRequest("Due unit must be hours or days".into()));
    }
    let to_group = list_group(pool, company_id, input.to_list_id).await?;
    if let Some(from) = input.from_list_id {
        if from == input.to_list_id {
            return Err(ApiError::BadRequest("Pick two different lists".into()));
        }
        if list_group(pool, company_id, from).await? != to_group {
            return Err(ApiError::BadRequest(
                "Both lists must be in the same group".into(),
            ));
        }
    }
    Ok(name.to_string())
}

/// List automatic activity rules
///
/// Every rule of the company, oldest first.
#[crate::openapi::path(
    get,
    path = "/v1/deal-stage-activity-rules",
    operation_id = "list_deal_stage_activity_rules",
    tag = "deals",
    responses(
        (status = 200, description = "Rules", body = Vec<ActivityRule>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<Vec<ActivityRule>>> {
    let rules = sqlx::query_as(
        "SELECT id, from_list_id, to_list_id, activity_name, due_amount,
                CAST(due_unit AS CHAR) AS due_unit, is_enabled
           FROM deal_stage_activity_rules
          WHERE company_id = ? AND deleted_at IS NULL
          ORDER BY id",
    )
    .bind(user.company_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(rules))
}

/// Add an automatic activity rule
#[crate::openapi::path(
    post,
    path = "/v1/deal-stage-activity-rules",
    operation_id = "create_deal_stage_activity_rule",
    tag = "deals",
    request_body = ActivityRuleInput,
    responses(
        (status = 201, description = "Created", body = CreatedId),
        (status = 400, description = "Missing name, bad due time, unknown or mismatched lists", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<ActivityRuleInput>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    let name = check_input(&state.pool, user.company_id, &input).await?;
    let id = sqlx::query(
        "INSERT INTO deal_stage_activity_rules
           (company_id, from_list_id, to_list_id, activity_name, due_amount, due_unit, is_enabled)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(user.company_id)
    .bind(input.from_list_id)
    .bind(input.to_list_id)
    .bind(name)
    .bind(input.due_amount)
    .bind(&input.due_unit)
    .bind(input.is_enabled.unwrap_or(true))
    .execute(&state.pool)
    .await?
    .last_insert_id();
    Ok((
        StatusCode::CREATED,
        Json(CreatedId {
            id: i64::try_from(id).unwrap_or(i64::MAX),
        }),
    ))
}

/// Change an automatic activity rule
///
/// Replaces every field. Activities it already added stay.
#[crate::openapi::path(
    put,
    path = "/v1/deal-stage-activity-rules/{id}",
    operation_id = "update_deal_stage_activity_rule",
    tag = "deals",
    params(("id" = i32, Path, description = "Rule id")),
    request_body = ActivityRuleInput,
    responses(
        (status = 204, description = "Changed"),
        (status = 400, description = "Missing name, bad due time, unknown or mismatched lists", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such rule of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<ActivityRuleInput>,
) -> ApiResult<StatusCode> {
    let name = check_input(&state.pool, user.company_id, &input).await?;
    let found = sqlx::query(
        "UPDATE deal_stage_activity_rules
            SET from_list_id = ?, to_list_id = ?, activity_name = ?, due_amount = ?,
                due_unit = ?, is_enabled = ?
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(input.from_list_id)
    .bind(input.to_list_id)
    .bind(name)
    .bind(input.due_amount)
    .bind(&input.due_unit)
    .bind(input.is_enabled.unwrap_or(true))
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?
    .rows_affected();
    // MySQL counts only changed rows: an unchanged save still exists.
    if found == 0 && !rule_exists(&state.pool, user.company_id, id).await? {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn rule_exists(pool: &MySqlPool, company_id: i32, id: i32) -> ApiResult<bool> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM deal_stage_activity_rules
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

/// Delete an automatic activity rule
///
/// Activities it already added stay.
#[crate::openapi::path(
    delete,
    path = "/v1/deal-stage-activity-rules/{id}",
    operation_id = "delete_deal_stage_activity_rule",
    tag = "deals",
    params(("id" = i32, Path, description = "Rule id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such rule of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    let done = sqlx::query(
        "UPDATE deal_stage_activity_rules SET deleted_at = NOW()
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?
    .rows_affected();
    if done == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}
