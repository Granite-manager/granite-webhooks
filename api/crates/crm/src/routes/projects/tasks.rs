//! Tasks, notes and files of a project. Anyone who may open the project adds
//! them; a task may go to anyone in the company, who is told at once and
//! again when it is due.

use super::access::Viewer;
use super::schemas::{NewProjectFiles, NoteInput, TaskInput, TaskTime, TaskTimeReset, TaskUpdate};
use super::service::visible_project;
use crate::auth::EmployeeUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::phone::crm_push::push_notification_logged;
use crate::routes::created::CreatedId;
use crate::routes::deals::service::reminder_due_at;
use crate::routes::uploads::schemas::UploadFolder;
use crate::routes::uploads::{attach, remove};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use chrono_tz::America::Indiana::Indianapolis;
use lambda_http::tracing;
use sqlx::MySqlConnection;
use std::collections::BTreeMap;

/// The bell: a task was given to you.
pub const TASK_ASSIGNED: &str = "project_task_assigned";
/// The bell, at the task's deadline.
pub const TASK_DUE: &str = "project_task_reminder";
/// The bell: someone finished a task you gave.
pub const TASK_DONE: &str = "project_task_done";

/// Date-only deadlines are reminded at this store time on the day.
const DATE_ONLY_REMINDER: &str = "09:00:00";

pub(super) fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// `YYYY-MM-DD` or `YYYY-MM-DD[T ]HH:MM[:SS]`, as stored (`YYYY-MM-DD HH:MM:SS`).
fn parse_deadline(raw: &str) -> ApiResult<Option<String>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let invalid = || ApiError::BadRequest("Pick a valid due date".into());
    let date = NaiveDate::parse_from_str(raw.get(..10).ok_or_else(invalid)?, "%Y-%m-%d")
        .map_err(|_| invalid())?;
    if raw.len() == 10 {
        return Ok(Some(format!("{date} 00:00:00")));
    }
    let separator = raw.get(10..11).ok_or_else(invalid)?;
    if separator != "T" && separator != " " {
        return Err(invalid());
    }
    let time = raw.get(11..).ok_or_else(invalid)?;
    let time = NaiveTime::parse_from_str(time, "%H:%M:%S")
        .or_else(|_| NaiveTime::parse_from_str(time, "%H:%M"))
        .map_err(|_| invalid())?;
    Ok(Some(format!("{date} {}", time.format("%H:%M:%S"))))
}

/// When a stored deadline is reminded: its time, or 9 AM store time on a
/// date with no time.
fn reminder_time(stored: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let (date, time) = stored.split_once(' ')?;
    let time = if time == "00:00:00" {
        DATE_ONLY_REMINDER
    } else {
        time
    };
    reminder_due_at(&format!("{date}T{time}"))
}

fn priority(raw: Option<&str>) -> ApiResult<&'static str> {
    match raw.map(str::trim).unwrap_or("medium") {
        "low" => Ok("low"),
        "" | "medium" => Ok("medium"),
        "high" => Ok("high"),
        _ => Err(ApiError::BadRequest(
            "Priority is low, medium or high".into(),
        )),
    }
}

async fn require_colleague(
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

fn actor_name(user: &SessionUser) -> Option<String> {
    let name = user.name.as_deref().unwrap_or_default().trim();
    (!name.is_empty()).then(|| clip(name, 255))
}

/// Tells the assignee (not the actor) and schedules the due reminder.
/// Logged, never returned: the task is saved.
async fn notify(
    state: &AppState,
    user: &SessionUser,
    project_id: i32,
    task_id: i32,
    name: &str,
    assignee: Option<i32>,
    deadline: Option<&str>,
) {
    let result: sqlx::Result<()> = async {
        let message = clip(name, 255);
        if let Some(assignee) = assignee.filter(|id| *id != user.id) {
            let id = sqlx::query(
                "INSERT INTO notifications
                    (user_id, project_id, project_task_id, message, notification_type, actor_name, due_at)
                 VALUES (?, ?, ?, ?, ?, ?, NOW())",
            )
            .bind(assignee)
            .bind(project_id)
            .bind(task_id)
            .bind(&message)
            .bind(TASK_ASSIGNED)
            .bind(actor_name(user))
            .execute(&state.pool)
            .await?
            .last_insert_id();
            push_notification_logged(state, i64::try_from(id).unwrap_or_default()).await;
        }
        if let Some(due) = deadline.and_then(reminder_time) {
            sqlx::query(
                "INSERT INTO notifications
                    (user_id, project_id, project_task_id, message, notification_type, actor_name, due_at)
                 VALUES (?, ?, ?, ?, ?, NULL, ?)",
            )
            .bind(assignee.unwrap_or(user.id))
            .bind(project_id)
            .bind(task_id)
            .bind(&message)
            .bind(TASK_DUE)
            .bind(due.naive_utc())
            .execute(&state.pool)
            .await?;
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        tracing::error!(?error, task_id, "project task notification failed");
    }
}

/// Tells the task's giver it is done (bell and phone). Logged, never
/// returned: the task is saved.
async fn tell_giver(
    state: &AppState,
    user: &SessionUser,
    project_id: i32,
    task_id: i32,
    giver: i32,
) {
    let result: sqlx::Result<()> = async {
        let id = sqlx::query(
            "INSERT INTO notifications
                (user_id, project_id, project_task_id, message, notification_type, actor_name, due_at)
             SELECT ?, ?, id, LEFT(name, 255), ?, ?, NOW() FROM project_activities WHERE id = ?",
        )
        .bind(giver)
        .bind(project_id)
        .bind(TASK_DONE)
        .bind(actor_name(user))
        .bind(task_id)
        .execute(&state.pool)
        .await?
        .last_insert_id();
        push_notification_logged(state, i64::try_from(id).unwrap_or_default()).await;
        Ok(())
    }
    .await;
    if let Err(error) = result {
        tracing::error!(?error, task_id, "project task done notification failed");
    }
}

/// Add a task
///
/// A task on the project for anyone in the company (or nobody). The person
/// gets a notification now and, with a due date, another when it is due (9 AM
/// store time on a date with no time).
#[crate::openapi::path(
    post,
    path = "/v1/projects/{id}/tasks",
    operation_id = "create_project_task",
    tag = "projects",
    params(("id" = i32, Path, description = "Project id")),
    request_body = TaskInput,
    responses(
        (status = 201, description = "Created", body = CreatedId),
        (status = 400, description = "Empty name, bad date or priority, or someone outside the company", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such project the caller may see", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_task(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<TaskInput>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    let name = input.name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest("Name the task".into()));
    }
    let name = clip(name, 255);
    let deadline = input
        .deadline
        .as_deref()
        .map(parse_deadline)
        .transpose()?
        .flatten();
    let priority = priority(input.priority.as_deref())?;
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    visible_project(&mut conn, &viewer, id).await?;
    if let Some(assignee) = input.assigned_user_id {
        require_colleague(&mut conn, viewer.company_id, assignee).await?;
    }
    let task = sqlx::query(
        "INSERT INTO project_activities
            (project_id, company_id, name, assigned_user_id, deadline, priority, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(viewer.company_id)
    .bind(&name)
    .bind(input.assigned_user_id)
    .bind(deadline.as_deref())
    .bind(priority)
    .bind(user.id)
    .execute(&mut *conn)
    .await?
    .last_insert_id();
    let task_id = i32::try_from(task).map_err(|_| ApiError::Internal("task id".into()))?;
    drop(conn);
    notify(
        &state,
        &user,
        id,
        task_id,
        &name,
        input.assigned_user_id,
        deadline.as_deref(),
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(CreatedId {
            id: i64::from(task_id),
        }),
    ))
}

/// How long one use gave the task: minutes, and whether it had a time of day.
/// A date with no time counts whole store days. A deadline already past when
/// it was set says nothing about the task.
fn use_minutes(created: DateTime<Utc>, deadline: NaiveDateTime) -> Option<(i64, bool)> {
    let wall = deadline.format("%Y-%m-%dT%H:%M:%S").to_string();
    let (minutes, has_time) = match reminder_due_at(&wall) {
        Some(due) => ((due - created).num_minutes(), true),
        None => {
            let given = created.with_timezone(&Indianapolis).date_naive();
            ((deadline.date() - given).num_days() * 24 * 60, false)
        }
    };
    (minutes >= 0).then_some((minutes, has_time))
}

/// One entry per task name (case aside): the average of its uses, oldest
/// first in `uses`.
fn average_times(uses: Vec<(String, DateTime<Utc>, NaiveDateTime)>) -> Vec<TaskTime> {
    let mut by_name: BTreeMap<String, (String, i64, i64, bool)> = BTreeMap::new();
    for (name, created, deadline) in uses {
        let Some((minutes, has_time)) = use_minutes(created, deadline) else {
            continue;
        };
        let entry = by_name
            .entry(name.trim().to_lowercase())
            .or_insert_with(|| (String::new(), 0, 0, false));
        entry.1 += minutes;
        entry.2 += 1;
        entry.0 = name;
        entry.3 = has_time;
    }
    by_name
        .into_values()
        .map(|(name, total, count, has_time)| TaskTime {
            name,
            minutes: (total + count / 2) / count,
            has_time,
        })
        .collect()
}

/// The company's remembered task times.
pub(super) async fn task_times(
    conn: &mut MySqlConnection,
    company_id: i32,
) -> sqlx::Result<Vec<TaskTime>> {
    let uses = sqlx::query_as(
        "SELECT name, created_at, deadline FROM project_activities
          WHERE company_id = ? AND deleted_at IS NULL AND deadline IS NOT NULL
            AND counts_for_time = 1
          ORDER BY id",
    )
    .bind(company_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(average_times(uses))
}

/// Forget a task's remembered time
///
/// Its earlier uses stop counting; the next one starts the average again.
#[crate::openapi::path(
    post,
    path = "/v1/projects/{id}/task-times/reset",
    operation_id = "reset_project_task_time",
    tag = "projects",
    params(("id" = i32, Path, description = "Project id")),
    request_body = TaskTimeReset,
    responses(
        (status = 204, description = "Forgotten"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such project the caller may see", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn reset_task_time(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<TaskTimeReset>,
) -> ApiResult<StatusCode> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    visible_project(&mut conn, &viewer, id).await?;
    sqlx::query(
        "UPDATE project_activities SET counts_for_time = 0
          WHERE company_id = ? AND LOWER(name) = LOWER(?)",
    )
    .bind(viewer.company_id)
    .bind(input.name.trim())
    .execute(&mut *conn)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The task, if the caller may open its project: (creator, assignee).
async fn task_people(
    conn: &mut MySqlConnection,
    viewer: &Viewer,
    project_id: i32,
    task_id: i32,
) -> ApiResult<(i32, Option<i32>)> {
    visible_project(conn, viewer, project_id).await?;
    sqlx::query_as(
        "SELECT created_by, assigned_user_id FROM project_activities
          WHERE id = ? AND project_id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(task_id)
    .bind(project_id)
    .bind(viewer.company_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(ApiError::NotFound)
}

/// Complete or reopen a task
///
/// Its creator, the person it is for, or a project manager. A done task's
/// due reminder leaves the bell; reopening brings it back.
#[crate::openapi::path(
    patch,
    path = "/v1/projects/{id}/tasks/{task_id}",
    operation_id = "update_project_task",
    tag = "projects",
    params(
        ("id" = i32, Path, description = "Project id"),
        ("task_id" = i32, Path, description = "Task id"),
    ),
    request_body = TaskUpdate,
    responses(
        (status = 204, description = "Saved"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not the task's creator, its person or a manager", body = ErrorBody),
        (status = 404, description = "No such task the caller may see", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_task(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, task_id)): Path<(i32, i32)>,
    JsonBody(input): JsonBody<TaskUpdate>,
) -> ApiResult<StatusCode> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    let (creator, assignee) = task_people(&mut conn, &viewer, id, task_id).await?;
    Viewer::require(viewer.permissions.manage || creator == user.id || assignee == Some(user.id))?;
    // Only an open task becomes done, so its giver hears it once.
    let finished = sqlx::query(
        "UPDATE project_activities SET completed_at = IF(?, UTC_TIMESTAMP(), NULL)
          WHERE id = ? AND company_id = ? AND (completed_at IS NULL) = ?",
    )
    .bind(input.completed)
    .bind(task_id)
    .bind(viewer.company_id)
    .bind(input.completed)
    .execute(&mut *conn)
    .await?
    .rows_affected()
        == 1
        && input.completed;
    if finished && creator != user.id {
        tell_giver(&state, &user, id, task_id, creator).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a task
///
/// Its creator or a project manager.
#[crate::openapi::path(
    delete,
    path = "/v1/projects/{id}/tasks/{task_id}",
    operation_id = "delete_project_task",
    tag = "projects",
    params(
        ("id" = i32, Path, description = "Project id"),
        ("task_id" = i32, Path, description = "Task id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not the task's creator or a manager", body = ErrorBody),
        (status = 404, description = "No such task the caller may see", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_task(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, task_id)): Path<(i32, i32)>,
) -> ApiResult<StatusCode> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    let (creator, _) = task_people(&mut conn, &viewer, id, task_id).await?;
    Viewer::require(viewer.permissions.manage || creator == user.id)?;
    sqlx::query(
        "UPDATE project_activities SET deleted_at = UTC_TIMESTAMP() WHERE id = ? AND company_id = ?",
    )
    .bind(task_id)
    .bind(viewer.company_id)
    .execute(&mut *conn)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Add a note
///
/// A note on the project, for everyone who can open it.
#[crate::openapi::path(
    post,
    path = "/v1/projects/{id}/notes",
    operation_id = "create_project_note",
    tag = "projects",
    params(("id" = i32, Path, description = "Project id")),
    request_body = NoteInput,
    responses(
        (status = 201, description = "Created", body = CreatedId),
        (status = 400, description = "Empty note", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such project the caller may see", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_note(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<NoteInput>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    let content = input.content.trim();
    if content.is_empty() {
        return Err(ApiError::BadRequest("Write the note first".into()));
    }
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    visible_project(&mut conn, &viewer, id).await?;
    let note = sqlx::query(
        "INSERT INTO project_notes (project_id, company_id, content, created_by) VALUES (?, ?, ?, ?)",
    )
    .bind(id)
    .bind(viewer.company_id)
    .bind(clip(content, 20_000))
    .bind(user.id)
    .execute(&mut *conn)
    .await?
    .last_insert_id();
    Ok((
        StatusCode::CREATED,
        Json(CreatedId {
            id: i64::try_from(note).unwrap_or_default(),
        }),
    ))
}

/// Delete a note
///
/// Its author or a project manager.
#[crate::openapi::path(
    delete,
    path = "/v1/projects/{id}/notes/{note_id}",
    operation_id = "delete_production_project_note",
    tag = "projects",
    params(
        ("id" = i32, Path, description = "Project id"),
        ("note_id" = i32, Path, description = "Note id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not the note's author or a manager", body = ErrorBody),
        (status = 404, description = "No such note the caller may see", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_note(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, note_id)): Path<(i32, i32)>,
) -> ApiResult<StatusCode> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    visible_project(&mut conn, &viewer, id).await?;
    let author: i32 = sqlx::query_scalar(
        "SELECT created_by FROM project_notes
          WHERE id = ? AND project_id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(note_id)
    .bind(id)
    .bind(viewer.company_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(ApiError::NotFound)?;
    Viewer::require(viewer.permissions.manage || author == user.id)?;
    sqlx::query("UPDATE project_notes SET deleted_at = UTC_TIMESTAMP() WHERE id = ?")
        .bind(note_id)
        .execute(&mut *conn)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Attach files
///
/// Files already uploaded with `POST /v1/uploads` (folder `deals`).
#[crate::openapi::path(
    post,
    path = "/v1/projects/{id}/files",
    operation_id = "attach_project_files",
    tag = "projects",
    params(("id" = i32, Path, description = "Project id")),
    request_body = NewProjectFiles,
    responses(
        (status = 204, description = "Attached"),
        (status = 400, description = "No files, or a URL that isn't the company's upload", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such project the caller may see", body = ErrorBody),
        (status = 500, description = "Database or storage error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn attach_files(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(body): JsonBody<NewProjectFiles>,
) -> ApiResult<StatusCode> {
    if body.files.is_empty() || body.files.len() > 20 {
        return Err(ApiError::BadRequest(
            "Attach 1 to 20 files at a time".into(),
        ));
    }
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    visible_project(&mut conn, &viewer, id).await?;
    for file in &body.files {
        attach(
            &state.storage,
            &file.url,
            UploadFolder::Deals,
            viewer.company_id,
        )
        .await?;
    }
    let mut tx = state.pool.begin().await?;
    for file in &body.files {
        let name = file.name.trim();
        let content_type = file
            .content_type
            .as_deref()
            .map(str::trim)
            .filter(|kind| !kind.is_empty())
            .unwrap_or("application/octet-stream");
        sqlx::query(
            "INSERT INTO project_files (project_id, company_id, name, url, content_type, created_by)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(viewer.company_id)
        .bind(clip(if name.is_empty() { "File" } else { name }, 255))
        .bind(&file.url)
        .bind(clip(content_type, 255))
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a file
///
/// Whoever attached it or a project manager.
#[crate::openapi::path(
    delete,
    path = "/v1/projects/{id}/files/{file_id}",
    operation_id = "delete_project_file",
    tag = "projects",
    params(
        ("id" = i32, Path, description = "Project id"),
        ("file_id" = i32, Path, description = "File id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not the file's uploader or a manager", body = ErrorBody),
        (status = 404, description = "No such file the caller may see", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_file(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, file_id)): Path<(i32, i32)>,
) -> ApiResult<StatusCode> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    visible_project(&mut conn, &viewer, id).await?;
    let (creator, url): (i32, String) = sqlx::query_as(
        "SELECT created_by, url FROM project_files
          WHERE id = ? AND project_id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(file_id)
    .bind(id)
    .bind(viewer.company_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(ApiError::NotFound)?;
    Viewer::require(viewer.permissions.manage || creator == user.id)?;
    sqlx::query("UPDATE project_files SET deleted_at = UTC_TIMESTAMP() WHERE id = ?")
        .bind(file_id)
        .execute(&mut *conn)
        .await?;
    drop(conn);
    remove(&state.storage, &url, viewer.company_id).await;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::{average_times, parse_deadline, priority, reminder_time};
    use chrono::{DateTime, NaiveDateTime, Utc};

    fn task_use(
        name: &str,
        created: &str,
        deadline: &str,
    ) -> (String, DateTime<Utc>, NaiveDateTime) {
        (
            name.into(),
            created.parse().unwrap(),
            NaiveDateTime::parse_from_str(deadline, "%Y-%m-%d %H:%M:%S").unwrap(),
        )
    }

    #[test]
    fn remembers_the_average_time_of_a_task() {
        // 10:00 store time to 14:00, then to 18:00; the name's case is ignored.
        let times = average_times(vec![
            task_use("Measure", "2026-11-02T15:00:00Z", "2026-11-02 14:00:00"),
            task_use("measure ", "2026-11-02T15:00:00Z", "2026-11-02 18:00:00"),
        ]);
        assert_eq!(times.len(), 1);
        assert_eq!(times[0].name, "measure ");
        assert_eq!(times[0].minutes, 360);
        assert!(times[0].has_time);
    }

    #[test]
    fn a_date_counts_whole_store_days_and_a_past_one_nothing() {
        // 2026-11-03 02:00 UTC is still Nov 2 in the store: three days to the 5th.
        let times = average_times(vec![
            task_use("Order sink", "2026-11-03T02:00:00Z", "2026-11-05 00:00:00"),
            task_use("Order sink", "2026-11-03T02:00:00Z", "2026-11-01 00:00:00"),
        ]);
        assert_eq!(times[0].minutes, 3 * 24 * 60);
        assert!(!times[0].has_time);
    }

    #[test]
    fn deadlines() {
        assert_eq!(
            parse_deadline("2026-11-02").unwrap().as_deref(),
            Some("2026-11-02 00:00:00")
        );
        assert_eq!(
            parse_deadline("2026-11-02T14:30").unwrap().as_deref(),
            Some("2026-11-02 14:30:00")
        );
        assert_eq!(
            parse_deadline("2026-11-02 14:30:15").unwrap().as_deref(),
            Some("2026-11-02 14:30:15")
        );
        assert_eq!(parse_deadline("  ").unwrap(), None);
        assert!(parse_deadline("2026-02-30").is_err());
        assert!(parse_deadline("2026-11-02x14:30").is_err());
    }

    #[test]
    fn reminders_fire_at_the_time_or_9_am_store_time() {
        // November: Indianapolis is UTC-5.
        assert_eq!(
            reminder_time("2026-11-02 14:30:00").map(|at| at.to_rfc3339()),
            Some("2026-11-02T19:30:00+00:00".into())
        );
        assert_eq!(
            reminder_time("2026-11-02 00:00:00").map(|at| at.to_rfc3339()),
            Some("2026-11-02T14:00:00+00:00".into())
        );
    }

    #[test]
    fn priorities() {
        assert_eq!(priority(None).unwrap(), "medium");
        assert_eq!(priority(Some("high")).unwrap(), "high");
        assert!(priority(Some("urgent")).is_err());
    }
}
