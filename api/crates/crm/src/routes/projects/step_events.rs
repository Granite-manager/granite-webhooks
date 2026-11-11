//! What checking a step off does to the project's schedule events
//! (Admin > Projects > Schedule): set their readiness and shop status.

use super::handlers::PROJECT_EVENTS;
use super::schemas::ProcessStep;
use crate::auth::AdminUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use sqlx::{MySqlConnection, QueryBuilder};

/// One event a step changes when it is checked off.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct StepEventRule {
    /// Which of the project's events: `templates`, `installations` or
    /// `event:<id>` (an event template of Admin > Quotes > Automation).
    pub target: String,
    /// New readiness: `not_ready`, `ready` or `confirmed`; null leaves it.
    #[serde(default)]
    pub readiness: Option<String>,
    /// New shop status: `cut` or `complete`; null leaves it.
    #[serde(default)]
    pub shop_status: Option<String>,
}

/// A step's new event rules; empty turns them off.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ProcessStepEvents {
    /// The rules, one per target.
    pub rules: Vec<StepEventRule>,
}

/// Fills each step's `event_rules`.
pub async fn load(conn: &mut MySqlConnection, steps: &mut [ProcessStep]) -> sqlx::Result<()> {
    if steps.is_empty() {
        return Ok(());
    }
    let ids: Vec<i32> = steps.iter().map(|step| step.id).collect();
    let mut sql = QueryBuilder::<sqlx::MySql>::new(
        "SELECT step_id, target, readiness, shop_status FROM project_process_step_events WHERE 1 = 1",
    );
    crate::sql::push_in(&mut sql, "step_id", &ids);
    sql.push(" ORDER BY id");
    let rows: Vec<(i32, String, Option<String>, Option<String>)> =
        sql.build_query_as().fetch_all(&mut *conn).await?;
    for (step_id, target, readiness, shop_status) in rows {
        if let Some(step) = steps.iter_mut().find(|step| step.id == step_id) {
            step.event_rules.push(StepEventRule {
                target,
                readiness,
                shop_status,
            });
        }
    }
    Ok(())
}

/// Set what checking a step off does to the project's events
///
/// Replaces the step's rules. Each names one kind of the project's events
/// (Template, Installation or one of the event templates) and the readiness
/// (Not ready, Ready, Confirmed) and/or shop status (Cut, Complete) to set.
#[crate::openapi::path(
    put,
    path = "/v1/projects/process-steps/{step_id}/events",
    operation_id = "set_project_process_step_events",
    tag = "projects",
    params(("step_id" = i32, Path, description = "Step id")),
    request_body = ProcessStepEvents,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Unknown event, readiness or shop status", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such step in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_events(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(step_id): Path<i32>,
    JsonBody(input): JsonBody<ProcessStepEvents>,
) -> ApiResult<StatusCode> {
    let mut conn = state.pool.acquire().await?;
    super::process::require_step(&mut conn, user.company_id, step_id).await?;
    let mut clean: Vec<StepEventRule> = Vec::new();
    for rule in input.rules {
        if clean.iter().any(|seen| seen.target == rule.target) {
            continue;
        }
        let readiness = rule.readiness.filter(|value| !value.is_empty());
        let shop_status = rule.shop_status.filter(|value| !value.is_empty());
        if readiness
            .as_deref()
            .is_some_and(|value| !["not_ready", "ready", "confirmed"].contains(&value))
        {
            return Err(ApiError::BadRequest(
                "Pick Not ready, Ready or Confirmed".into(),
            ));
        }
        if shop_status
            .as_deref()
            .is_some_and(|value| !["cut", "complete"].contains(&value))
        {
            return Err(ApiError::BadRequest("Pick Cut or Complete".into()));
        }
        if readiness.is_none() && shop_status.is_none() {
            return Err(ApiError::BadRequest("Pick a status to set".into()));
        }
        match rule.target.strip_prefix("event:") {
            Some(id) => {
                let found: Option<i32> = sqlx::query_scalar(
                    "SELECT id FROM quote_sale_event_templates WHERE id = ? AND company_id = ?",
                )
                .bind(id.parse::<i32>().unwrap_or(0))
                .bind(user.company_id)
                .fetch_optional(&mut *conn)
                .await?;
                if found.is_none() {
                    return Err(ApiError::BadRequest(
                        "Pick one of the event templates".into(),
                    ));
                }
            }
            None if ["templates", "installations"].contains(&rule.target.as_str()) => {}
            None => return Err(ApiError::BadRequest("Pick which event to change".into())),
        }
        clean.push(StepEventRule {
            target: rule.target,
            readiness,
            shop_status,
        });
    }
    // The pool holds one connection: free ours before the transaction takes it.
    drop(conn);
    let mut tx = state.pool.begin().await?;
    sqlx::query("DELETE FROM project_process_step_events WHERE step_id = ?")
        .bind(step_id)
        .execute(&mut *tx)
        .await?;
    for rule in &clean {
        sqlx::query(
            "INSERT INTO project_process_step_events (step_id, target, readiness, shop_status)
             VALUES (?, ?, ?, ?)",
        )
        .bind(step_id)
        .bind(&rule.target)
        .bind(&rule.readiness)
        .bind(&rule.shop_status)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Applies the step's rules to the project's events. Logged, never returned:
/// the check-off is saved.
pub(super) async fn apply(
    conn: &mut MySqlConnection,
    actor_id: Option<i32>,
    project_id: i32,
    step_id: i32,
) {
    if let Err(error) = apply_rules(conn, actor_id, project_id, step_id).await {
        tracing::error!(?error, project_id, step_id, "step event rules failed");
    }
}

async fn apply_rules(
    conn: &mut MySqlConnection,
    actor_id: Option<i32>,
    project_id: i32,
    step_id: i32,
) -> sqlx::Result<()> {
    let rules: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT target, readiness, shop_status FROM project_process_step_events WHERE step_id = ?",
    )
    .bind(step_id)
    .fetch_all(&mut *conn)
    .await?;
    if rules.is_empty() {
        return Ok(());
    }
    let company_id: Option<i32> =
        sqlx::query_scalar("SELECT company_id FROM project_process_steps WHERE id = ?")
            .bind(step_id)
            .fetch_optional(&mut *conn)
            .await?;
    let Some(company_id) = company_id else {
        return Ok(());
    };
    for (target, readiness, shop_status) in rules {
        // The calendar the events are on, and the text their title ends with.
        let (slug, calendar_id, ending): (Option<String>, Option<i32>, String) =
            match target.strip_prefix("event:") {
                Some(id) => {
                    let template: Option<(i32, String)> = sqlx::query_as(
                        "SELECT calendar_id, title_text FROM quote_sale_event_templates
                          WHERE id = ? AND company_id = ?",
                    )
                    .bind(id.parse::<i32>().unwrap_or(0))
                    .bind(company_id)
                    .fetch_optional(&mut *conn)
                    .await?;
                    let Some((calendar_id, ending)) = template else {
                        continue;
                    };
                    (None, Some(calendar_id), ending)
                }
                None => (Some(target.clone()), None, String::new()),
            };
        let sql = format!(
            "UPDATE events e
               JOIN projects p ON p.id = ?
               JOIN calendars cal ON cal.id = e.calendar_id AND cal.company_id = ?
                                 AND cal.deleted_at IS NULL
                SET e.status = CASE WHEN ? IS NULL THEN e.status
                                    WHEN ? = 'confirmed' AND e.unscheduled <> 0 THEN 'ready'
                                    ELSE ? END,
                    e.shop_status = IF(? IS NULL, e.shop_status, ?),
                    e.shop_status_at = IF(? IS NULL, e.shop_status_at, UTC_TIMESTAMP()),
                    e.shop_status_by = IF(? IS NULL, e.shop_status_by, ?)
              WHERE {PROJECT_EVENTS}
                AND (cal.slug = ? OR e.calendar_id = ?)
                AND RIGHT(e.title, CHAR_LENGTH(?)) = ?"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(project_id)
            .bind(company_id)
            .bind(&readiness)
            .bind(&readiness)
            .bind(&readiness)
            .bind(&shop_status)
            .bind(&shop_status)
            .bind(&shop_status)
            .bind(&shop_status)
            .bind(actor_id)
            .bind(slug)
            .bind(calendar_id)
            .bind(&ending)
            .bind(&ending)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}
