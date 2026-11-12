//! A company's own steps in the Projects process: admins add them to a stage
//! (`/admin/projects`), a project in that stage shows the first one not done
//! as its Next, and anyone who can see the project checks it off.

use super::access::{Stage, Viewer};
use super::schemas::{
    DoneStep, NotifyPosition, Person, ProcessStage, ProcessStep, ProcessStepAuto, ProcessStepInput,
    ProcessStepName, ProcessStepNotify, ProcessStepOrder, StepAuto, StepDone,
};
use super::service::visible_project;
use crate::auth::{AdminUser, EmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::phone::crm_push::push_notification_logged;
use crate::routes::created::CreatedId;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use lambda_http::tracing;
use sqlx::{MySql, MySqlConnection, QueryBuilder};

const NAME_MAX: usize = 120;

/// The bell: someone checked off a step you are told about.
pub const STEP_DONE: &str = "project_step_done";

/// The name to store, or why it can't be.
fn clean_name(name: &str) -> ApiResult<&str> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest("Name the step".into()));
    }
    if name.chars().count() > NAME_MAX {
        return Err(ApiError::BadRequest(format!(
            "A step's name can be {NAME_MAX} characters at most"
        )));
    }
    Ok(name)
}

/// The company's steps, in process order.
pub async fn company_steps(
    conn: &mut MySqlConnection,
    company_id: i32,
) -> sqlx::Result<Vec<ProcessStep>> {
    let mut steps: Vec<ProcessStep> = sqlx::query_as(
        "SELECT id, stage_code, name, position, notify_sales_rep FROM project_process_steps
          WHERE company_id = ?
          ORDER BY FIELD(stage_code, 'new', 'templated', 'fabrication', 'installed'), position, id",
    )
    .bind(company_id)
    .fetch_all(&mut *conn)
    .await?;
    let told: Vec<(i32, i32)> = sqlx::query_as(
        "SELECT n.step_id, n.user_id FROM project_process_step_notify n
           JOIN project_process_steps s ON s.id = n.step_id AND s.company_id = ?
           JOIN users u ON u.id = n.user_id AND u.is_deleted = 0
          ORDER BY u.name, u.id",
    )
    .bind(company_id)
    .fetch_all(&mut *conn)
    .await?;
    for (step_id, user_id) in told {
        if let Some(step) = steps.iter_mut().find(|step| step.id == step_id) {
            step.notify.push(user_id);
        }
    }
    let rules: Vec<(
        i32,
        String,
        Option<i32>,
        Option<String>,
        Option<String>,
        Option<i32>,
    )> = sqlx::query_as(
        "SELECT id, auto_trigger, auto_ref_id, auto_ref_ids, auto_text, auto_days FROM project_process_steps
          WHERE company_id = ? AND auto_trigger IS NOT NULL",
    )
    .bind(company_id)
    .fetch_all(&mut *conn)
    .await?;
    for (step_id, trigger, ref_id, ref_ids, text, days) in rules {
        if let Some(step) = steps.iter_mut().find(|step| step.id == step_id) {
            let mut ids: Vec<i32> = ref_ids
                .iter()
                .flat_map(|list| list.split(','))
                .filter_map(|id| id.trim().parse().ok())
                .collect();
            if ids.is_empty() {
                ids.extend(ref_id);
            }
            step.auto = Some(StepAuto {
                trigger,
                ref_id,
                ref_ids: ids,
                text,
                days,
            });
        }
    }
    super::step_events::load(&mut *conn, &mut steps).await?;
    Ok(steps)
}

/// The steps these projects have done.
pub async fn done_steps(
    conn: &mut MySqlConnection,
    project_ids: &[i32],
) -> sqlx::Result<Vec<DoneStep>> {
    if project_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut sql = QueryBuilder::<MySql>::new(
        "SELECT d.project_id, d.step_id, NULLIF(d.part_id, 0) AS part_id,
                IF(d.auto = 1, 'Auto', u.name) AS done_by,
                DATE_FORMAT(d.done_at, '%Y-%m-%dT%H:%i:%sZ') AS done_at
           FROM project_process_steps_done d
           LEFT JOIN users u ON u.id = d.done_by
          WHERE 1 = 1",
    );
    crate::sql::push_in(&mut sql, "d.project_id", project_ids);
    sql.build_query_as().fetch_all(&mut *conn).await
}

/// The company's process steps
#[crate::openapi::path(
    get,
    path = "/v1/projects/process-steps",
    operation_id = "list_project_process_steps",
    tag = "projects",
    responses(
        (status = 200, description = "The steps, in process order", body = Vec<ProcessStep>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<Vec<ProcessStep>>> {
    let mut conn = state.pool.acquire().await?;
    Ok(Json(company_steps(&mut conn, user.company_id).await?))
}

/// Add a process step
///
/// Goes last in its stage. Paid & Completed has nothing after it, so it takes none.
#[crate::openapi::path(
    post,
    path = "/v1/projects/process-steps",
    operation_id = "create_project_process_step",
    tag = "projects",
    request_body = ProcessStepInput,
    responses(
        (status = 201, description = "Added", body = CreatedId),
        (status = 400, description = "Missing or long name, or Paid & Completed", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<ProcessStepInput>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    let name = clean_name(&input.name)?;
    if input.stage == Stage::Completed {
        return Err(ApiError::BadRequest(
            "Paid & Completed is the end of the process, so it has no next step".into(),
        ));
    }
    let id = sqlx::query(
        "INSERT INTO project_process_steps (company_id, stage_code, name, position)
         SELECT ?, ?, ?, COALESCE(MAX(position) + 1, 0)
           FROM project_process_steps WHERE company_id = ? AND stage_code = ?",
    )
    .bind(user.company_id)
    .bind(input.stage.as_str())
    .bind(name)
    .bind(user.company_id)
    .bind(input.stage.as_str())
    .execute(&state.pool)
    .await?
    .last_insert_id();
    Ok((StatusCode::CREATED, Json(CreatedId { id: id as i64 })))
}

/// Stops here unless the step is the company's.
pub(super) async fn require_step(
    conn: &mut MySqlConnection,
    company_id: i32,
    step_id: i32,
) -> ApiResult<()> {
    sqlx::query_scalar::<_, i32>(
        "SELECT id FROM project_process_steps WHERE id = ? AND company_id = ?",
    )
    .bind(step_id)
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(ApiError::NotFound)?;
    Ok(())
}

/// Rename a process step
#[crate::openapi::path(
    patch,
    path = "/v1/projects/process-steps/{step_id}",
    operation_id = "rename_project_process_step",
    tag = "projects",
    params(("step_id" = i32, Path, description = "Step id")),
    request_body = ProcessStepName,
    responses(
        (status = 204, description = "Renamed"),
        (status = 400, description = "Missing or long name", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such step in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn rename(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(step_id): Path<i32>,
    JsonBody(input): JsonBody<ProcessStepName>,
) -> ApiResult<StatusCode> {
    let name = clean_name(&input.name)?;
    let mut conn = state.pool.acquire().await?;
    require_step(&mut conn, user.company_id, step_id).await?;
    sqlx::query("UPDATE project_process_steps SET name = ? WHERE id = ? AND company_id = ?")
        .bind(name)
        .bind(step_id)
        .bind(user.company_id)
        .execute(&mut *conn)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a process step
///
/// Gone from every project, with its check-offs.
#[crate::openapi::path(
    delete,
    path = "/v1/projects/process-steps/{step_id}",
    operation_id = "delete_project_process_step",
    tag = "projects",
    params(("step_id" = i32, Path, description = "Step id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such step in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(step_id): Path<i32>,
) -> ApiResult<StatusCode> {
    let deleted = sqlx::query("DELETE FROM project_process_steps WHERE id = ? AND company_id = ?")
        .bind(step_id)
        .bind(user.company_id)
        .execute(&state.pool)
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Set who is told when a step is checked off
///
/// Replaces the list. Each person gets a bell notification and a phone push
/// when someone checks the step off a project (not when they do it themselves).
#[crate::openapi::path(
    put,
    path = "/v1/projects/process-steps/{step_id}/notify",
    operation_id = "set_project_process_step_notify",
    tag = "projects",
    params(("step_id" = i32, Path, description = "Step id")),
    request_body = ProcessStepNotify,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Someone outside the company", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such step in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_notify(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(step_id): Path<i32>,
    JsonBody(input): JsonBody<ProcessStepNotify>,
) -> ApiResult<StatusCode> {
    let mut ids = input.user_ids;
    ids.sort_unstable();
    ids.dedup();
    let mut conn = state.pool.acquire().await?;
    require_step(&mut conn, user.company_id, step_id).await?;
    if !ids.is_empty() {
        let mut sql = QueryBuilder::<MySql>::new(
            "SELECT COUNT(*) FROM users WHERE is_deleted = 0 AND company_id = ",
        );
        sql.push_bind(user.company_id);
        crate::sql::push_in(&mut sql, "id", &ids);
        let found: i64 = sql.build_query_scalar().fetch_one(&mut *conn).await?;
        if usize::try_from(found).unwrap_or_default() != ids.len() {
            return Err(ApiError::BadRequest("Pick people from your company".into()));
        }
    }
    // The pool holds one connection: free ours before the transaction takes it.
    drop(conn);
    let mut tx = state.pool.begin().await?;
    sqlx::query("UPDATE project_process_steps SET notify_sales_rep = ? WHERE id = ?")
        .bind(input.sales_rep)
        .bind(step_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM project_process_step_notify WHERE step_id = ?")
        .bind(step_id)
        .execute(&mut *tx)
        .await?;
    for id in &ids {
        sqlx::query("INSERT INTO project_process_step_notify (step_id, user_id) VALUES (?, ?)")
            .bind(step_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The stages that end with a built-in step ("Then: ..."), and its name.
const BUILT_IN_STEPS: [(Stage, &str); 4] = [
    (Stage::New, "Schedule the template"),
    (Stage::Templated, "Start fabrication"),
    (Stage::Fabrication, "Schedule the installation"),
    (Stage::Installed, "Confirm full payment"),
];

/// Who is told when a project moves on from each stage.
pub async fn company_stages(
    conn: &mut MySqlConnection,
    company_id: i32,
) -> sqlx::Result<Vec<ProcessStage>> {
    let told: Vec<(String, i32)> = sqlx::query_as(
        "SELECT n.stage_code, n.user_id FROM project_stage_notify n
           JOIN users u ON u.id = n.user_id AND u.is_deleted = 0 AND u.company_id = n.company_id
          WHERE n.company_id = ?
          ORDER BY u.name, u.id",
    )
    .bind(company_id)
    .fetch_all(&mut *conn)
    .await?;
    let reps: Vec<String> =
        sqlx::query_scalar("SELECT stage_code FROM project_stage_notify_rep WHERE company_id = ?")
            .bind(company_id)
            .fetch_all(&mut *conn)
            .await?;
    Ok(BUILT_IN_STEPS
        .iter()
        .map(|(stage, _)| ProcessStage {
            stage_code: *stage,
            notify: told
                .iter()
                .filter(|(code, _)| code == stage.as_str())
                .map(|(_, user_id)| *user_id)
                .collect(),
            notify_sales_rep: reps.iter().any(|code| code == stage.as_str()),
        })
        .collect())
}

/// Who is told when a project moves on from each stage
///
/// The built-in line at the end of each stage ("Then: Start fabrication")
/// takes Notify like a step does.
#[crate::openapi::path(
    get,
    path = "/v1/projects/process-stages",
    operation_id = "list_project_process_stages",
    tag = "projects",
    responses(
        (status = 200, description = "Who is told, per stage", body = Vec<ProcessStage>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_stages(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<Vec<ProcessStage>>> {
    let mut conn = state.pool.acquire().await?;
    Ok(Json(company_stages(&mut conn, user.company_id).await?))
}

/// Set who is told when a project moves on from a stage
///
/// Replaces the list. Each person gets a bell notification and a phone push
/// when someone moves a project on from the stage (not when they do it
/// themselves), by hand, by checking its last step or from a booking.
#[crate::openapi::path(
    put,
    path = "/v1/projects/process-stages/{stage}/notify",
    operation_id = "set_project_process_stage_notify",
    tag = "projects",
    params(("stage" = String, Path, description = "The stage: new, templated, fabrication or installed")),
    request_body = ProcessStepNotify,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Someone outside the company", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such stage", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_stage_notify(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(stage): Path<String>,
    JsonBody(input): JsonBody<ProcessStepNotify>,
) -> ApiResult<StatusCode> {
    let stage = Stage::try_from(stage).map_err(|_| ApiError::NotFound)?;
    if !BUILT_IN_STEPS
        .iter()
        .any(|(built_in, _)| *built_in == stage)
    {
        return Err(ApiError::NotFound);
    }
    let mut ids = input.user_ids;
    ids.sort_unstable();
    ids.dedup();
    if !ids.is_empty() {
        let mut sql = QueryBuilder::<MySql>::new(
            "SELECT COUNT(*) FROM users WHERE is_deleted = 0 AND company_id = ",
        );
        sql.push_bind(user.company_id);
        crate::sql::push_in(&mut sql, "id", &ids);
        let found: i64 = sql.build_query_scalar().fetch_one(&state.pool).await?;
        if usize::try_from(found).unwrap_or_default() != ids.len() {
            return Err(ApiError::BadRequest("Pick people from your company".into()));
        }
    }
    let mut tx = state.pool.begin().await?;
    sqlx::query("DELETE FROM project_stage_notify WHERE company_id = ? AND stage_code = ?")
        .bind(user.company_id)
        .bind(stage.as_str())
        .execute(&mut *tx)
        .await?;
    for id in &ids {
        sqlx::query(
            "INSERT INTO project_stage_notify (company_id, stage_code, user_id) VALUES (?, ?, ?)",
        )
        .bind(user.company_id)
        .bind(stage.as_str())
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("DELETE FROM project_stage_notify_rep WHERE company_id = ? AND stage_code = ?")
        .bind(user.company_id)
        .bind(stage.as_str())
        .execute(&mut *tx)
        .await?;
    if input.sales_rep {
        sqlx::query("INSERT INTO project_stage_notify_rep (company_id, stage_code) VALUES (?, ?)")
            .bind(user.company_id)
            .bind(stage.as_str())
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Tells the people of every stage a project just moved on from (the moves
/// added to its stage history after `seen`), not the one who moved it.
/// Logged, never returned: the move is saved.
pub(super) async fn tell_moved_on(
    state: &AppState,
    actor_id: i32,
    actor_name: Option<&str>,
    company_id: i32,
    project_id: i32,
    seen: i64,
) {
    let result: sqlx::Result<()> = async {
        let moves: Vec<(String, Option<i32>)> = sqlx::query_as(
            "SELECT from_stage, part_id FROM project_stage_history
              WHERE project_id = ? AND id > ? AND from_stage IS NOT NULL
                AND FIELD(to_stage, 'new', 'templated', 'fabrication', 'installed', 'completed')
                  > FIELD(from_stage, 'new', 'templated', 'fabrication', 'installed', 'completed')
              ORDER BY id",
        )
        .bind(project_id)
        .bind(seen)
        .fetch_all(&state.pool)
        .await?;
        for (from, part_id) in moves {
            let Some((stage, name)) = BUILT_IN_STEPS
                .iter()
                .find(|(stage, _)| stage.as_str() == from)
            else {
                continue;
            };
            tell_stage_people(
                state,
                (actor_id, actor_name),
                (company_id, project_id, part_id.unwrap_or(0)),
                *stage,
                name,
            )
            .await?;
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        tracing::error!(?error, project_id, "project stage notification failed");
    }
}

async fn tell_stage_people(
    state: &AppState,
    (actor_id, actor_name): (i32, Option<&str>),
    (company_id, project_id, part_id): (i32, i32, i32),
    stage: Stage,
    name: &str,
) -> sqlx::Result<()> {
    let people: Vec<i32> = sqlx::query_scalar(
        "SELECT n.user_id FROM project_stage_notify n
           JOIN users u ON u.id = n.user_id AND u.is_deleted = 0
          WHERE n.company_id = ? AND n.stage_code = ? AND n.user_id <> ?
         UNION
         SELECT p.sales_rep_id FROM projects p
           JOIN project_stage_notify_rep r ON r.company_id = p.company_id AND r.stage_code = ?
           JOIN users u ON u.id = p.sales_rep_id AND u.is_deleted = 0
          WHERE p.id = ? AND p.sales_rep_id <> ?",
    )
    .bind(company_id)
    .bind(stage.as_str())
    .bind(actor_id)
    .bind(stage.as_str())
    .bind(project_id)
    .bind(actor_id)
    .fetch_all(&state.pool)
    .await?;
    let actor = actor_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| name.chars().take(255).collect::<String>());
    for person in people {
        let id = sqlx::query(
            "INSERT INTO notifications
                (user_id, project_id, message, notification_type, actor_name, due_at)
             VALUES (?, ?,
                     LEFT(CONCAT(?, COALESCE((SELECT CONCAT(' (', pp.name, ')')
                                                FROM project_parts pp WHERE pp.id = ?), '')),
                          255),
                     ?, ?, NOW())",
        )
        .bind(person)
        .bind(project_id)
        .bind(name)
        .bind(part_id)
        .bind(STEP_DONE)
        .bind(&actor)
        .execute(&state.pool)
        .await?
        .last_insert_id();
        push_notification_logged(state, i64::try_from(id).unwrap_or_default()).await;
    }
    Ok(())
}

/// The positions Notify can pick from
///
/// Every position that has a Projects board, with the company's people in it,
/// so a step can tell specific Sales Reps, Shop Managers, Scheduling Managers
/// and so on.
#[crate::openapi::path(
    get,
    path = "/v1/projects/process-steps/positions",
    operation_id = "list_project_step_positions",
    tag = "projects",
    responses(
        (status = 200, description = "The positions and their people", body = Vec<NotifyPosition>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn notify_positions(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<Vec<NotifyPosition>>> {
    let rows: Vec<(i32, i32, String)> = sqlx::query_as(
        "SELECT up.position_id, u.id, COALESCE(NULLIF(TRIM(u.name), ''), CONCAT('User ', u.id))
           FROM users_positions up
           JOIN users u ON u.id = up.user_id AND u.is_deleted = 0 AND u.company_id = ?
          WHERE up.company_id = ?
          ORDER BY 3, u.id",
    )
    .bind(user.company_id)
    .bind(user.company_id)
    .fetch_all(&state.pool)
    .await?;
    let positions = NOTIFY_POSITIONS
        .iter()
        .map(|(id, name)| NotifyPosition {
            id: *id,
            name: (*name).to_string(),
            people: rows
                .iter()
                .filter(|row| row.0 == *id)
                .map(|row| Person {
                    id: row.1,
                    name: row.2.clone(),
                })
                .collect(),
        })
        .collect();
    Ok(Json(positions))
}

/// The positions with a Projects board (and Shop Worker), by `positions.id`.
const NOTIFY_POSITIONS: [(i32, &str); 7] = [
    (1, "Sales Rep"),
    (2, "Sales Manager"),
    (5, "Manager"),
    (3, "Shop Manager"),
    (4, "Shop Worker"),
    (12, "Inventory Manager"),
    (13, "Scheduling Manager"),
];

/// What can check a step off by itself.
pub const TRIGGERS: [&str; 14] = [
    "email_template",
    "text_template",
    "customer_replied",
    "file_added",
    "sent_to_sign",
    "signed",
    "questionnaire_filled",
    "template_booked",
    "install_booked",
    "invoice_sent",
    "payment_any",
    "payment_full",
    "task_done",
    "days_after",
];

/// The rule to store, or why it can't be.
async fn clean_auto(
    conn: &mut MySqlConnection,
    company_id: i32,
    auto: StepAuto,
) -> ApiResult<StepAuto> {
    let trigger = auto.trigger.trim().to_string();
    if !TRIGGERS.contains(&trigger.as_str()) {
        return Err(ApiError::BadRequest("Pick what checks the step off".into()));
    }
    let mut clean = StepAuto {
        trigger,
        ref_id: None,
        ref_ids: Vec::new(),
        text: None,
        days: None,
    };
    match clean.trigger.as_str() {
        "email_template" | "text_template" => {
            let table = if clean.trigger == "email_template" {
                "email_templates"
            } else {
                "cloudtalk_sms_templates"
            };
            let mut wanted = auto.ref_ids.clone();
            wanted.extend(auto.ref_id);
            wanted.sort_unstable();
            wanted.dedup();
            let mut found: Vec<i32> = Vec::new();
            for id in wanted {
                let ok: Option<i32> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "SELECT id FROM {table} WHERE id = ? AND company_id = ? AND deleted_at IS NULL"
                )))
                .bind(id)
                .bind(company_id)
                .fetch_optional(&mut *conn)
                .await?;
                found.extend(ok);
            }
            if found.is_empty() {
                return Err(ApiError::BadRequest("Pick a template".into()));
            }
            clean.ref_id = found.first().copied();
            clean.ref_ids = found;
        }
        "sent_to_sign" | "signed" | "task_done" => {
            let text = auto
                .text
                .as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(|text| text.chars().take(NAME_MAX).collect::<String>());
            if clean.trigger == "task_done" && text.is_none() {
                return Err(ApiError::BadRequest("Name the task".into()));
            }
            clean.text = text;
        }
        "days_after" => {
            let days = auto.days.filter(|days| (1..=365).contains(days));
            clean.days =
                Some(days.ok_or_else(|| ApiError::BadRequest("Pick 1 to 365 days".into()))?);
        }
        _ => {}
    }
    Ok(clean)
}

/// Set what checks a step off by itself
///
/// Replaces the step's rule; `null` turns it off. Only what happens after it is
/// set counts, and only once the step before it is done.
#[crate::openapi::path(
    put,
    path = "/v1/projects/process-steps/{step_id}/auto",
    operation_id = "set_project_process_step_auto",
    tag = "projects",
    params(("step_id" = i32, Path, description = "Step id")),
    request_body = ProcessStepAuto,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Unknown action, or a missing template, task name or days", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such step in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_auto(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(step_id): Path<i32>,
    JsonBody(input): JsonBody<ProcessStepAuto>,
) -> ApiResult<StatusCode> {
    let mut conn = state.pool.acquire().await?;
    require_step(&mut conn, user.company_id, step_id).await?;
    let rule = match input.auto {
        Some(auto) => Some(clean_auto(&mut conn, user.company_id, auto).await?),
        None => None,
    };
    sqlx::query(
        "UPDATE project_process_steps
            SET auto_trigger = ?, auto_ref_id = ?, auto_ref_ids = ?, auto_text = ?, auto_days = ?,
                auto_since = IF(? IS NULL, NULL, UTC_TIMESTAMP(3))
          WHERE id = ? AND company_id = ?",
    )
    .bind(rule.as_ref().map(|rule| rule.trigger.clone()))
    .bind(rule.as_ref().and_then(|rule| rule.ref_id))
    .bind(
        rule.as_ref()
            .filter(|rule| !rule.ref_ids.is_empty())
            .map(|rule| {
                rule.ref_ids
                    .iter()
                    .map(i32::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            }),
    )
    .bind(rule.as_ref().and_then(|rule| rule.text.clone()))
    .bind(rule.as_ref().and_then(|rule| rule.days))
    .bind(rule.as_ref().map(|rule| rule.trigger.clone()))
    .bind(step_id)
    .bind(user.company_id)
    .execute(&mut *conn)
    .await?;
    // A new rule starts fresh on every project.
    sqlx::query("DELETE FROM project_process_step_auto_off WHERE step_id = ?")
        .bind(step_id)
        .execute(&mut *conn)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
/// Tells the step's people (not the one who checked it) on the bell and
/// their phones. Logged, never returned: the check-off is saved.
pub(super) async fn tell_step_people(
    state: &AppState,
    actor_id: Option<i32>,
    actor_name: Option<&str>,
    project_id: i32,
    step_id: i32,
    part_id: i32,
) {
    let result: sqlx::Result<()> = async {
        let people: Vec<i32> = sqlx::query_scalar(
            "SELECT n.user_id FROM project_process_step_notify n
               JOIN users u ON u.id = n.user_id AND u.is_deleted = 0
              WHERE n.step_id = ? AND n.user_id <> ?
             UNION
             SELECT p.sales_rep_id FROM projects p
               JOIN project_process_steps s ON s.id = ? AND s.notify_sales_rep = 1
               JOIN users u ON u.id = p.sales_rep_id AND u.is_deleted = 0
              WHERE p.id = ? AND p.sales_rep_id <> ?",
        )
        .bind(step_id)
        .bind(actor_id.unwrap_or(0))
        .bind(step_id)
        .bind(project_id)
        .bind(actor_id.unwrap_or(0))
        .fetch_all(&state.pool)
        .await?;
        let actor = actor_name
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(|name| name.chars().take(255).collect::<String>());
        for person in people {
            let id = sqlx::query(
                "INSERT INTO notifications
                    (user_id, project_id, message, notification_type, actor_name, due_at)
                 SELECT ?, ?,
                        LEFT(CONCAT(name, COALESCE((SELECT CONCAT(' (', pp.name, ')')
                                                      FROM project_parts pp WHERE pp.id = ?), '')),
                             255),
                        ?, ?, NOW()
                   FROM project_process_steps WHERE id = ?",
            )
            .bind(person)
            .bind(project_id)
            .bind(part_id)
            .bind(STEP_DONE)
            .bind(&actor)
            .bind(step_id)
            .execute(&state.pool)
            .await?
            .last_insert_id();
            push_notification_logged(state, i64::try_from(id).unwrap_or_default()).await;
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        tracing::error!(
            ?error,
            project_id,
            step_id,
            "project step notification failed"
        );
    }
}

/// Save a stage's order of steps
#[crate::openapi::path(
    post,
    path = "/v1/projects/process-steps/reorder",
    operation_id = "reorder_project_process_steps",
    tag = "projects",
    request_body = ProcessStepOrder,
    responses(
        (status = 204, description = "Saved"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn reorder(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<ProcessStepOrder>,
) -> ApiResult<StatusCode> {
    let mut tx = state.pool.begin().await?;
    for (position, id) in input.ids.iter().enumerate() {
        sqlx::query(
            "UPDATE project_process_steps SET position = ? WHERE id = ? AND company_id = ?",
        )
        .bind(i32::try_from(position).unwrap_or(i32::MAX))
        .bind(id)
        .bind(user.company_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Check a step off a project, or undo it
///
/// Anyone who can see the project, while it is not archived or cancelled.
#[crate::openapi::path(
    put,
    path = "/v1/projects/{id}/steps/{step_id}",
    operation_id = "set_project_step_done",
    tag = "projects",
    params(
        ("id" = i32, Path, description = "Project id"),
        ("step_id" = i32, Path, description = "Step id"),
    ),
    request_body = StepDone,
    responses(
        (status = 204, description = "Saved"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 404, description = "No such project the caller may see, or step in the company", body = ErrorBody),
        (status = 409, description = "The project is archived or cancelled", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_done(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, step_id)): Path<(i32, i32)>,
    JsonBody(input): JsonBody<StepDone>,
) -> ApiResult<StatusCode> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    let project = visible_project(&mut conn, &viewer, id).await?;
    if project.archived_at.is_some()
        || project.cancelled_at.is_some()
        || project.sale_cancelled_at.is_some()
    {
        return Err(ApiError::Conflict(
            "This project is archived or cancelled, so its steps can't change.".into(),
        ));
    }
    require_step(&mut conn, viewer.company_id, step_id).await?;
    // On a project with parts a step is done for one part (0: no parts).
    let parts = super::parts::parts_of(&mut conn, viewer.company_id, &[id]).await?;
    let part = if parts.is_empty() {
        0
    } else {
        super::parts::target(&parts, input.part_id)?.id
    };
    if input.done {
        let added = sqlx::query(
            "INSERT IGNORE INTO project_process_steps_done (project_id, step_id, part_id, done_by)
             VALUES (?, ?, ?, ?)",
        )
        .bind(id)
        .bind(step_id)
        .bind(part)
        .bind(user.id)
        .execute(&mut *conn)
        .await?
        .rows_affected();
        // Checked by hand: its rule may check it again after a later uncheck.
        sqlx::query(
            "DELETE FROM project_process_step_auto_off
              WHERE project_id = ? AND step_id = ? AND part_id = ?",
        )
        .bind(id)
        .bind(step_id)
        .bind(part)
        .execute(&mut *conn)
        .await?;
        if added > 0 {
            super::step_events::apply(&mut conn, Some(user.id), id, step_id, part).await;
        }
        drop(conn);
        // Already done (a second click) tells nobody again.
        if added > 0 {
            tell_step_people(
                &state,
                Some(user.id),
                user.name.as_deref(),
                id,
                step_id,
                part,
            )
            .await;
        }
    } else {
        sqlx::query(
            "DELETE FROM project_process_steps_done
              WHERE project_id = ? AND step_id = ? AND part_id = ?",
        )
        .bind(id)
        .bind(step_id)
        .bind(part)
        .execute(&mut *conn)
        .await?;
        // Unchecked by hand: its rule leaves it alone on this project.
        sqlx::query(
            "INSERT IGNORE INTO project_process_step_auto_off (project_id, step_id, part_id)
             SELECT ?, id, ? FROM project_process_steps WHERE id = ? AND auto_trigger IS NOT NULL",
        )
        .bind(id)
        .bind(part)
        .bind(step_id)
        .execute(&mut *conn)
        .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}
