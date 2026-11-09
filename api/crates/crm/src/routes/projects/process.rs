//! A company's own steps in the Projects process: admins add them to a stage
//! (`/admin/projects`), a project in that stage shows the first one not done
//! as its Next, and anyone who can see the project checks it off.

use super::access::{Stage, Viewer};
use super::schemas::{
    DoneStep, ProcessStep, ProcessStepInput, ProcessStepName, ProcessStepOrder, StepDone,
};
use super::service::visible_project;
use crate::auth::{AdminUser, EmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::routes::created::CreatedId;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use sqlx::{MySql, MySqlConnection, QueryBuilder};

const NAME_MAX: usize = 120;

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
    sqlx::query_as(
        "SELECT id, stage_code, name, position FROM project_process_steps
          WHERE company_id = ?
          ORDER BY FIELD(stage_code, 'new', 'templated', 'fabrication', 'installed'), position, id",
    )
    .bind(company_id)
    .fetch_all(&mut *conn)
    .await
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
        "SELECT d.project_id, d.step_id, u.name AS done_by,
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
async fn require_step(conn: &mut MySqlConnection, company_id: i32, step_id: i32) -> ApiResult<()> {
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
    if input.done {
        sqlx::query(
            "INSERT IGNORE INTO project_process_steps_done (project_id, step_id, done_by)
             VALUES (?, ?, ?)",
        )
        .bind(id)
        .bind(step_id)
        .bind(user.id)
        .execute(&mut *conn)
        .await?;
    } else {
        sqlx::query("DELETE FROM project_process_steps_done WHERE project_id = ? AND step_id = ?")
            .bind(id)
            .bind(step_id)
            .execute(&mut *conn)
            .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}
