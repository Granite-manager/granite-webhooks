//! Who does each step and the company's lists (owner, 2026-11-16).
//!
//! - A step is someone's: a person, or the project's sales rep, set in Admin ›
//!   Projects; one project can pick another person. It is due `due_days`
//!   after the project reaches its stage.
//! - A step can have its own to-do list; ticking its last item checks the
//!   step off.
//! - The company keeps lists (Admin › Projects) to fill a to-do list task.

use super::access::{Stage, Viewer};
use super::schemas::{
    ItemDone, ListItem, Project, ProjectList, ProjectListInput, StepItemDone, StepNote,
    StepNoteInput, StepOwner, StepOwnerInput, StepSetup,
};
use super::service::visible_project;
use super::tasks::{clip, list_items};
use crate::auth::{AdminUser, EmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::phone::crm_push::push_notification_logged;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use chrono_tz::America::Indiana::Indianapolis;
use lambda_http::tracing;
use sqlx::MySqlConnection;

/// The bell: a step of a project was given to you.
pub const STEP_ASSIGNED: &str = "project_step_assigned";

const LIST_NAME_MAX: usize = 120;

/// The company's lists, in order, each with its items.
pub(super) async fn company_lists(
    conn: &mut MySqlConnection,
    company_id: i32,
) -> sqlx::Result<Vec<ProjectList>> {
    let lists: Vec<(i32, String)> = sqlx::query_as(
        "SELECT id, name FROM project_lists
          WHERE company_id = ? AND deleted_at IS NULL ORDER BY position, id",
    )
    .bind(company_id)
    .fetch_all(&mut *conn)
    .await?;
    let items: Vec<(i32, i32, String)> = sqlx::query_as(
        "SELECT i.list_id, i.id, i.label FROM project_list_items i
           JOIN project_lists l ON l.id = i.list_id AND l.company_id = ? AND l.deleted_at IS NULL
          ORDER BY i.position, i.id",
    )
    .bind(company_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(lists
        .into_iter()
        .map(|(id, name)| ProjectList {
            items: items
                .iter()
                .filter(|(list, _, _)| *list == id)
                .map(|(_, item_id, label)| ListItem {
                    id: *item_id,
                    label: label.clone(),
                })
                .collect(),
            id,
            name,
        })
        .collect())
}

fn list_name(name: &str) -> ApiResult<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest("Name the list".into()));
    }
    if name.chars().count() > LIST_NAME_MAX {
        return Err(ApiError::BadRequest(
            "A list's name can be 120 characters at most".into(),
        ));
    }
    Ok(name.to_string())
}

async fn write_list_items(
    conn: &mut MySqlConnection,
    list_id: i32,
    items: &[String],
) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM project_list_items WHERE list_id = ?")
        .bind(list_id)
        .execute(&mut *conn)
        .await?;
    for (position, label) in items.iter().enumerate() {
        sqlx::query("INSERT INTO project_list_items (list_id, label, position) VALUES (?, ?, ?)")
            .bind(list_id)
            .bind(label)
            .bind(i32::try_from(position).unwrap_or(i32::MAX))
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// The company's lists
///
/// The lists that fill a new to-do list task, in order. Anyone in the company.
#[crate::openapi::path(
    get,
    path = "/v1/projects/lists",
    operation_id = "list_project_lists",
    tag = "projects",
    responses(
        (status = 200, description = "The lists", body = Vec<ProjectList>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_lists(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Vec<ProjectList>>> {
    let mut conn = state.pool.acquire().await?;
    Ok(Json(company_lists(&mut conn, user.company_id).await?))
}

/// Add a list
///
/// Admins. Returns every list.
#[crate::openapi::path(
    post,
    path = "/v1/projects/lists",
    operation_id = "create_project_list",
    tag = "projects",
    request_body = ProjectListInput,
    responses(
        (status = 201, description = "Created; every list", body = Vec<ProjectList>),
        (status = 400, description = "Missing or long name, or a long item", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_list(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<ProjectListInput>,
) -> ApiResult<(StatusCode, Json<Vec<ProjectList>>)> {
    let name = list_name(&input.name)?;
    let items = list_items(&input.items)?;
    let mut tx = state.pool.begin().await?;
    let id = sqlx::query(
        "INSERT INTO project_lists (company_id, name, position)
         SELECT ?, ?, COALESCE(MAX(position) + 1, 0) FROM project_lists WHERE company_id = ?",
    )
    .bind(user.company_id)
    .bind(&name)
    .bind(user.company_id)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    write_list_items(&mut tx, i32::try_from(id).unwrap_or_default(), &items).await?;
    tx.commit().await?;
    let mut conn = state.pool.acquire().await?;
    Ok((
        StatusCode::CREATED,
        Json(company_lists(&mut conn, user.company_id).await?),
    ))
}

/// Change a list
///
/// Admins. Replaces its name and items; to-do lists already made from it
/// stay as they are. Returns every list.
#[crate::openapi::path(
    put,
    path = "/v1/projects/lists/{list_id}",
    operation_id = "update_project_list",
    tag = "projects",
    params(("list_id" = i32, Path, description = "List id")),
    request_body = ProjectListInput,
    responses(
        (status = 200, description = "Saved; every list", body = Vec<ProjectList>),
        (status = 400, description = "Missing or long name, or a long item", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such list in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_list(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(list_id): Path<i32>,
    JsonBody(input): JsonBody<ProjectListInput>,
) -> ApiResult<Json<Vec<ProjectList>>> {
    let name = list_name(&input.name)?;
    let items = list_items(&input.items)?;
    let mut tx = state.pool.begin().await?;
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM project_lists WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(list_id)
    .bind(user.company_id)
    .fetch_optional(&mut *tx)
    .await?;
    found.ok_or(ApiError::NotFound)?;
    sqlx::query("UPDATE project_lists SET name = ? WHERE id = ?")
        .bind(&name)
        .bind(list_id)
        .execute(&mut *tx)
        .await?;
    write_list_items(&mut tx, list_id, &items).await?;
    tx.commit().await?;
    let mut conn = state.pool.acquire().await?;
    Ok(Json(company_lists(&mut conn, user.company_id).await?))
}

/// Delete a list
///
/// Admins. To-do lists already made from it stay.
#[crate::openapi::path(
    delete,
    path = "/v1/projects/lists/{list_id}",
    operation_id = "delete_project_list",
    tag = "projects",
    params(("list_id" = i32, Path, description = "List id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such list in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_list(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(list_id): Path<i32>,
) -> ApiResult<StatusCode> {
    let deleted = sqlx::query(
        "UPDATE project_lists SET deleted_at = UTC_TIMESTAMP(3)
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(list_id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?
    .rows_affected();
    if deleted == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Set who does a step, when it is due, and its own to-do list
///
/// Admins. `responsible_sales_rep` makes it the project's sales rep's;
/// otherwise `responsible_user_id` (or nobody). Items keep their ticks when
/// sent back with their `id`.
#[crate::openapi::path(
    put,
    path = "/v1/projects/process-steps/{step_id}/setup",
    operation_id = "set_project_process_step_setup",
    tag = "projects",
    params(("step_id" = i32, Path, description = "Step id")),
    request_body = StepSetup,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Someone outside the company, bad days, or a long item", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such step in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_step_setup(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(step_id): Path<i32>,
    JsonBody(input): JsonBody<StepSetup>,
) -> ApiResult<StatusCode> {
    if input
        .due_days
        .is_some_and(|days| !(0..=365).contains(&days))
    {
        return Err(ApiError::BadRequest("Pick 0 to 365 days".into()));
    }
    let labels: Vec<String> = input.items.iter().map(|item| item.label.clone()).collect();
    list_items(&labels)?;
    let mut tx = state.pool.begin().await?;
    super::process::require_step(&mut tx, user.company_id, step_id).await?;
    let responsible = if input.responsible_sales_rep {
        None
    } else {
        input.responsible_user_id
    };
    if let Some(person) = responsible {
        require_person(&mut tx, user.company_id, person).await?;
    }
    sqlx::query(
        "UPDATE project_process_steps
            SET responsible_user_id = ?, responsible_sales_rep = ?, due_days = ?
          WHERE id = ? AND company_id = ?",
    )
    .bind(responsible)
    .bind(input.responsible_sales_rep)
    .bind(input.due_days)
    .bind(step_id)
    .bind(user.company_id)
    .execute(&mut *tx)
    .await?;
    let kept: Vec<i32> = input.items.iter().filter_map(|item| item.id).collect();
    let existing: Vec<i32> =
        sqlx::query_scalar("SELECT id FROM project_step_items WHERE step_id = ?")
            .bind(step_id)
            .fetch_all(&mut *tx)
            .await?;
    for id in existing.iter().filter(|id| !kept.contains(id)) {
        sqlx::query("DELETE FROM project_step_items WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    let mut position = 0;
    for item in &input.items {
        let label = item.label.trim();
        if label.is_empty() {
            continue;
        }
        let label = clip(label, 200);
        match item.id.filter(|id| existing.contains(id)) {
            Some(id) => {
                sqlx::query("UPDATE project_step_items SET label = ?, position = ? WHERE id = ?")
                    .bind(&label)
                    .bind(position)
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            }
            None => {
                sqlx::query(
                    "INSERT INTO project_step_items (step_id, label, position) VALUES (?, ?, ?)",
                )
                .bind(step_id)
                .bind(&label)
                .bind(position)
                .execute(&mut *tx)
                .await?;
            }
        }
        position += 1;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn require_person(
    conn: &mut MySqlConnection,
    company_id: i32,
    user_id: i32,
) -> ApiResult<()> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM users WHERE id = ? AND company_id = ? AND is_deleted = 0",
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?;
    found
        .map(|_| ())
        .ok_or_else(|| ApiError::BadRequest("Pick someone in your company".into()))
}

/// When the project reached each stage it has reached.
async fn stages_reached(
    conn: &mut MySqlConnection,
    project: &Project,
) -> sqlx::Result<Vec<(Stage, NaiveDateTime)>> {
    // Both columns are TIMESTAMP (UTC).
    let rows: Vec<(String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT to_stage, MAX(created_at) FROM project_stage_history
          WHERE project_id = ? GROUP BY to_stage",
    )
    .bind(project.id)
    .fetch_all(&mut *conn)
    .await?;
    let created: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT created_at FROM projects WHERE id = ?")
            .bind(project.id)
            .fetch_optional(&mut *conn)
            .await?;
    let mut reached: Vec<(Stage, NaiveDateTime)> = rows
        .into_iter()
        .filter_map(|(stage, at)| {
            Stage::try_from(stage)
                .ok()
                .map(|stage| (stage, at.naive_utc()))
        })
        .filter(|(stage, _)| *stage <= project.stage_code)
        .collect();
    if let Some(created) = created
        && !reached.iter().any(|(stage, _)| *stage == Stage::New)
    {
        reached.push((Stage::New, created.naive_utc()));
    }
    // A stage passed over (moved two at once) counts from the next one reached.
    for stage in Stage::ALL {
        if stage > project.stage_code || reached.iter().any(|(s, _)| *s == stage) {
            continue;
        }
        if let Some(at) = reached
            .iter()
            .filter(|(s, _)| *s > stage)
            .map(|(_, at)| *at)
            .min()
        {
            reached.push((stage, at));
        }
    }
    Ok(reached)
}

/// Who does each step on this project, and when it is due.
pub(super) async fn step_owners(
    conn: &mut MySqlConnection,
    project: &Project,
    steps: &[super::schemas::ProcessStep],
) -> sqlx::Result<Vec<StepOwner>> {
    let picked: Vec<(i32, i32, String)> = sqlx::query_as(
        "SELECT o.step_id, o.user_id, COALESCE(NULLIF(TRIM(u.name), ''), u.email)
           FROM project_step_owners o JOIN users u ON u.id = o.user_id
          WHERE o.project_id = ?",
    )
    .bind(project.id)
    .fetch_all(&mut *conn)
    .await?;
    let rule_ids: Vec<i32> = steps.iter().filter_map(|s| s.responsible_user_id).collect();
    let mut names: Vec<(i32, String)> = Vec::new();
    if !rule_ids.is_empty() {
        let mut sql = sqlx::QueryBuilder::<sqlx::MySql>::new(
            "SELECT id, COALESCE(NULLIF(TRIM(name), ''), email) FROM users WHERE is_deleted = 0",
        );
        crate::sql::push_in(&mut sql, "id", &rule_ids);
        names = sql.build_query_as().fetch_all(&mut *conn).await?;
    }
    let reached = stages_reached(conn, project).await?;
    Ok(steps
        .iter()
        .map(|step| {
            let own = picked.iter().find(|(step_id, _, _)| *step_id == step.id);
            let (user_id, user_name) = match own {
                Some((_, id, name)) => (Some(*id), Some(name.clone())),
                None if step.responsible_sales_rep => {
                    (project.sales_rep_id, project.sales_rep_name.clone())
                }
                None => match step.responsible_user_id {
                    Some(id) => (
                        names.iter().find(|(n, _)| *n == id).map(|(id, _)| *id),
                        names
                            .iter()
                            .find(|(n, _)| *n == id)
                            .map(|(_, name)| name.clone()),
                    ),
                    None => (None, None),
                },
            };
            let due = step.due_days.and_then(|days| {
                reached
                    .iter()
                    .find(|(stage, _)| *stage == step.stage_code)
                    .map(|(_, at)| {
                        (at.and_utc().with_timezone(&Indianapolis).date_naive()
                            + Duration::days(i64::from(days)))
                        .format("%Y-%m-%d")
                        .to_string()
                    })
            });
            StepOwner {
                step_id: step.id,
                user_id,
                user_name,
                picked: own.is_some(),
                due,
            }
        })
        .collect())
}

/// Ticks of the steps' own to-do lists on these projects.
pub(super) async fn step_items_done(
    conn: &mut MySqlConnection,
    project_id: i32,
) -> sqlx::Result<Vec<StepItemDone>> {
    sqlx::query_as(
        "SELECT d.item_id, NULLIF(d.part_id, 0) AS part_id, u.name AS done_by,
                DATE_FORMAT(d.done_at, '%Y-%m-%dT%H:%i:%sZ') AS done_at
           FROM project_step_items_done d LEFT JOIN users u ON u.id = d.done_by
          WHERE d.project_id = ?",
    )
    .bind(project_id)
    .fetch_all(&mut *conn)
    .await
}

/// The steps' notes on a project.
pub(super) async fn step_notes(
    conn: &mut MySqlConnection,
    project_id: i32,
) -> sqlx::Result<Vec<StepNote>> {
    sqlx::query_as(
        "SELECT n.step_id, NULLIF(n.part_id, 0) AS part_id, n.note,
                COALESCE(NULLIF(TRIM(u.name), ''), u.email) AS updated_by,
                DATE_FORMAT(n.updated_at, '%Y-%m-%dT%H:%i:%sZ') AS updated_at
           FROM project_step_notes n LEFT JOIN users u ON u.id = n.updated_by
          WHERE n.project_id = ?",
    )
    .bind(project_id)
    .fetch_all(&mut *conn)
    .await
}

/// Write a step's note on a project
///
/// Optional, like the step's files (owner, 2026-11-18). Anyone who can see
/// the project, while it is not archived or cancelled. An empty note removes it.
#[crate::openapi::path(
    put,
    path = "/v1/projects/{id}/steps/{step_id}/note",
    operation_id = "set_project_step_note",
    tag = "projects",
    params(
        ("id" = i32, Path, description = "Project id"),
        ("step_id" = i32, Path, description = "Step id"),
    ),
    request_body = StepNoteInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "A note over 5,000 characters, or a part not on the project", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 404, description = "No such project the caller may see, or step in the company", body = ErrorBody),
        (status = 409, description = "The project is archived or cancelled", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_step_note(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, step_id)): Path<(i32, i32)>,
    JsonBody(input): JsonBody<StepNoteInput>,
) -> ApiResult<StatusCode> {
    let note = input.note.trim();
    if note.chars().count() > 5000 {
        return Err(ApiError::BadRequest(
            "A step's note can be 5,000 characters at most".into(),
        ));
    }
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
    super::process::require_step(&mut conn, viewer.company_id, step_id).await?;
    let parts = super::parts::parts_of(&mut conn, viewer.company_id, &[id]).await?;
    let part = if parts.is_empty() {
        0
    } else {
        super::parts::target(&parts, input.part_id)?.id
    };
    if note.is_empty() {
        sqlx::query(
            "DELETE FROM project_step_notes WHERE project_id = ? AND step_id = ? AND part_id = ?",
        )
        .bind(id)
        .bind(step_id)
        .bind(part)
        .execute(&mut *conn)
        .await?;
    } else {
        sqlx::query(
            "INSERT INTO project_step_notes (project_id, step_id, part_id, note, updated_by, updated_at)
             VALUES (?, ?, ?, ?, ?, UTC_TIMESTAMP(3))
             ON DUPLICATE KEY UPDATE note = VALUES(note), updated_by = VALUES(updated_by),
                                     updated_at = VALUES(updated_at)",
        )
        .bind(id)
        .bind(step_id)
        .bind(part)
        .bind(note)
        .bind(user.id)
        .execute(&mut *conn)
        .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The people involved in a project: its sales rep and owner, the people on
/// its steps and tasks, the crew on its appointments, and its questions' people.
pub(super) async fn involved(
    conn: &mut MySqlConnection,
    project: &Project,
    owners: &[StepOwner],
) -> sqlx::Result<Vec<i32>> {
    let mut ids: Vec<i32> = sqlx::query_scalar(
        "SELECT assigned_user_id FROM project_activities
          WHERE project_id = ? AND deleted_at IS NULL AND assigned_user_id IS NOT NULL
         UNION SELECT created_by FROM project_activities WHERE project_id = ? AND deleted_at IS NULL
         UNION SELECT assigned_user_id FROM events
          WHERE project_id = ? AND deleted_date IS NULL AND assigned_user_id IS NOT NULL
         UNION SELECT m.user_id FROM project_question_members m
           JOIN project_questions q ON q.id = m.question_id WHERE q.project_id = ?",
    )
    .bind(project.id)
    .bind(project.id)
    .bind(project.id)
    .bind(project.id)
    .fetch_all(&mut *conn)
    .await?;
    ids.extend(project.sales_rep_id);
    ids.extend(project.owner_id);
    ids.extend(owners.iter().filter_map(|owner| owner.user_id));
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

/// Pick who does a step on this project
///
/// Anyone who can open the project. `null` goes back to the company's rule.
/// The person gets a bell notification and a phone push (unless it is the caller).
#[crate::openapi::path(
    put,
    path = "/v1/projects/{id}/steps/{step_id}/owner",
    operation_id = "set_project_step_owner",
    tag = "projects",
    params(
        ("id" = i32, Path, description = "Project id"),
        ("step_id" = i32, Path, description = "Step id"),
    ),
    request_body = StepOwnerInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Someone outside the company", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 404, description = "No such project the caller may see, or step in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_step_owner(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, step_id)): Path<(i32, i32)>,
    JsonBody(input): JsonBody<StepOwnerInput>,
) -> ApiResult<StatusCode> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    visible_project(&mut conn, &viewer, id).await?;
    super::process::require_step(&mut conn, viewer.company_id, step_id).await?;
    match input.user_id {
        Some(person) => {
            require_person(&mut conn, viewer.company_id, person).await?;
            sqlx::query(
                "INSERT INTO project_step_owners (project_id, step_id, user_id) VALUES (?, ?, ?)
                 ON DUPLICATE KEY UPDATE user_id = VALUES(user_id)",
            )
            .bind(id)
            .bind(step_id)
            .bind(person)
            .execute(&mut *conn)
            .await?;
        }
        None => {
            sqlx::query("DELETE FROM project_step_owners WHERE project_id = ? AND step_id = ?")
                .bind(id)
                .bind(step_id)
                .execute(&mut *conn)
                .await?;
        }
    }
    drop(conn);
    if let Some(person) = input.user_id.filter(|person| *person != user.id) {
        let result: sqlx::Result<()> = async {
            let notification = sqlx::query(
                "INSERT INTO notifications
                    (user_id, project_id, message, notification_type, actor_name, due_at)
                 SELECT ?, ?, LEFT(name, 255), ?, ?, NOW() FROM project_process_steps WHERE id = ?",
            )
            .bind(person)
            .bind(id)
            .bind(STEP_ASSIGNED)
            .bind(user.name.as_deref())
            .bind(step_id)
            .execute(&state.pool)
            .await?
            .last_insert_id();
            push_notification_logged(&state, i64::try_from(notification).unwrap_or_default()).await;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            tracing::error!(?error, step_id, "project step owner notification failed");
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Tick a step's to-do list item off, or open it again
///
/// Anyone who can see the project, while it is not archived or cancelled.
/// Ticking the step's last open item checks the step off; opening one again
/// opens the step again.
#[crate::openapi::path(
    put,
    path = "/v1/projects/{id}/step-items/{item_id}",
    operation_id = "set_project_step_item_done",
    tag = "projects",
    params(
        ("id" = i32, Path, description = "Project id"),
        ("item_id" = i32, Path, description = "Step item id"),
    ),
    request_body = ItemDone,
    responses(
        (status = 204, description = "Saved"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 404, description = "No such project the caller may see, or item in the company", body = ErrorBody),
        (status = 409, description = "The project is archived or cancelled", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_step_item_done(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, item_id)): Path<(i32, i32)>,
    JsonBody(input): JsonBody<ItemDone>,
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
    let step_id: i32 = sqlx::query_scalar(
        "SELECT i.step_id FROM project_step_items i
           JOIN project_process_steps s ON s.id = i.step_id AND s.company_id = ?
          WHERE i.id = ?",
    )
    .bind(viewer.company_id)
    .bind(item_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(ApiError::NotFound)?;
    let parts = super::parts::parts_of(&mut conn, viewer.company_id, &[id]).await?;
    let part = if parts.is_empty() {
        0
    } else {
        super::parts::target(&parts, input.part_id)?.id
    };
    if !input.done {
        sqlx::query(
            "DELETE FROM project_step_items_done WHERE project_id = ? AND item_id = ? AND part_id = ?",
        )
        .bind(id)
        .bind(item_id)
        .bind(part)
        .execute(&mut *conn)
        .await?;
        // A step with a list is done only while every item is: one open opens it again.
        sqlx::query(
            "DELETE FROM project_process_steps_done
              WHERE project_id = ? AND step_id = ? AND part_id = ?",
        )
        .bind(id)
        .bind(step_id)
        .bind(part)
        .execute(&mut *conn)
        .await?;
        return Ok(StatusCode::NO_CONTENT);
    }
    sqlx::query(
        "INSERT IGNORE INTO project_step_items_done (project_id, item_id, part_id, done_by)
         VALUES (?, ?, ?, ?)",
    )
    .bind(id)
    .bind(item_id)
    .bind(part)
    .bind(user.id)
    .execute(&mut *conn)
    .await?;
    let open: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM project_step_items i
          WHERE i.step_id = ? AND NOT EXISTS (
                SELECT 1 FROM project_step_items_done d
                 WHERE d.item_id = i.id AND d.project_id = ? AND d.part_id = ?)",
    )
    .bind(step_id)
    .bind(id)
    .bind(part)
    .fetch_one(&mut *conn)
    .await?;
    if open > 0 {
        return Ok(StatusCode::NO_CONTENT);
    }
    // The last item: the step is done, as if checked off by hand.
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
    if added > 0 {
        super::step_events::apply(&mut conn, Some(user.id), id, step_id, part).await;
        drop(conn);
        super::process::tell_step_people(
            &state,
            Some(user.id),
            user.name.as_deref(),
            id,
            step_id,
            part,
        )
        .await;
    }
    Ok(StatusCode::NO_CONTENT)
}
