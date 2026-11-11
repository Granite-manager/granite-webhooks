//! Issues on a project: the crew's, reported on its jobs (`event_issues` of
//! its events), and the office's, reported on the project page
//! (`event_issues.project_id`, no job). Anyone who sees the project marks any
//! of them solved.
//!
//! - `POST   /v1/projects/{id}/issues`             → report one (text, pictures)
//! - `PUT    /v1/projects/{id}/issues/{issue_id}`  → solved, or open again
//! - `DELETE /v1/projects/{id}/issues/{issue_id}`  → the reporter or an admin, project issues only

use super::access::Viewer;
use super::handlers::PROJECT_EVENTS;
use super::service::visible_project;
use crate::auth::EmployeeUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::routes::uploads::schemas::UploadFolder;
use crate::routes::uploads::{attach, remove};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;
use sqlx::{MySqlConnection, MySqlPool};

/// The longest description, in characters (as on a job).
const MAX_DESCRIPTION_CHARS: usize = 2000;
/// The most pictures on one issue (as on a job).
const MAX_PICTURES: usize = 20;

/// Body of `POST /v1/projects/{id}/issues`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ProjectIssueInput {
    /// What is wrong: 1 to 2000 characters (trimmed).
    pub description: String,
    /// Pictures: `url`s that `POST /v1/uploads` (folder `events`) returned.
    /// At most 20; may be empty.
    #[serde(default)]
    pub urls: Vec<String>,
}

/// Body of `PUT .../issues/{issue_id}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct IssueSolvedInput {
    /// Solved, or open again.
    pub solved: bool,
}

/// Marks the company's issue solved (or open again) where `filter` (one `?`,
/// bound to `filter_id`) holds; `true` when there is such an issue. Solved
/// keeps who did it first and when.
pub async fn mark_issue_solved(
    pool: &MySqlPool,
    user: &SessionUser,
    issue_id: i32,
    solved: bool,
    filter: &str,
    filter_id: i32,
) -> sqlx::Result<bool> {
    let there: Option<i32> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT id FROM event_issues
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL AND {filter}"
    )))
    .bind(issue_id)
    .bind(user.company_id)
    .bind(filter_id)
    .fetch_optional(pool)
    .await?;
    if there.is_none() {
        return Ok(false);
    }
    if solved {
        sqlx::query(
            "UPDATE event_issues SET solved_at = NOW(), solved_by = ?, solved_by_name = ?
              WHERE id = ? AND solved_at IS NULL",
        )
        .bind(user.id)
        .bind(&user.name)
        .bind(issue_id)
        .execute(pool)
        .await?;
    } else {
        sqlx::query(
            "UPDATE event_issues SET solved_at = NULL, solved_by = NULL, solved_by_name = NULL
              WHERE id = ?",
        )
        .bind(issue_id)
        .execute(pool)
        .await?;
    }
    Ok(true)
}

/// Stops here unless the issue is on the project (its own, or one of its jobs').
async fn require_issue(
    conn: &mut MySqlConnection,
    project_id: i32,
    issue_id: i32,
) -> ApiResult<()> {
    let found: Option<i32> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT ei.id FROM event_issues ei
           JOIN projects p ON p.id = ? AND p.company_id = ei.company_id
          WHERE ei.id = ? AND ei.deleted_at IS NULL
            AND (ei.project_id = p.id
                 OR EXISTS (SELECT 1 FROM events e WHERE e.id = ei.event_id AND {PROJECT_EVENTS}))"
    )))
    .bind(project_id)
    .bind(issue_id)
    .fetch_optional(&mut *conn)
    .await?;
    found.map(|_| ()).ok_or(ApiError::NotFound)
}

/// Report an issue on a project
///
/// For a problem with no job on the schedule: a description and, optionally,
/// pictures uploaded to the `events` folder first. Anyone who can see the
/// project, while it is not archived or cancelled.
#[crate::openapi::path(
    post,
    path = "/v1/projects/{id}/issues",
    operation_id = "report_project_issue",
    tag = "projects",
    params(("id" = i32, Path, description = "Project id")),
    request_body = ProjectIssueInput,
    responses(
        (status = 201, description = "Reported"),
        (status = 400, description = "No description, one over 2000 characters, over 20 pictures, or a URL that was not uploaded", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 404, description = "No such project the caller may see", body = ErrorBody),
        (status = 409, description = "The project is archived or cancelled", body = ErrorBody),
        (status = 500, description = "Database or storage error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn report(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<ProjectIssueInput>,
) -> ApiResult<StatusCode> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    let project = visible_project(&mut conn, &viewer, id).await?;
    if project.archived_at.is_some()
        || project.cancelled_at.is_some()
        || project.sale_cancelled_at.is_some()
    {
        return Err(ApiError::Conflict(
            "This project is archived or cancelled, so it takes no new issues.".into(),
        ));
    }
    drop(conn);
    let description = input.description.trim();
    if description.is_empty() {
        return Err(ApiError::BadRequest("Describe the issue".into()));
    }
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(ApiError::BadRequest(
            "The description must be 2000 characters or less".into(),
        ));
    }
    if input.urls.len() > MAX_PICTURES {
        return Err(ApiError::BadRequest("At most 20 pictures".into()));
    }
    for url in &input.urls {
        attach(&state.storage, url, UploadFolder::Events, user.company_id).await?;
    }
    let mut tx = state.pool.begin().await?;
    let issue_id = sqlx::query(
        "INSERT INTO event_issues (event_id, project_id, company_id, description, created_by, created_by_name)
         VALUES (NULL, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(user.company_id)
    .bind(description)
    .bind(user.id)
    .bind(&user.name)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    for url in &input.urls {
        sqlx::query("INSERT INTO event_issue_images (issue_id, url) VALUES (?, ?)")
            .bind(issue_id)
            .bind(url)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::CREATED)
}

/// Mark a project's issue solved, or open again
///
/// Any issue on the project, the crew's included. Anyone who can see it.
#[crate::openapi::path(
    put,
    path = "/v1/projects/{id}/issues/{issue_id}",
    operation_id = "set_project_issue_solved",
    tag = "projects",
    params(
        ("id" = i32, Path, description = "Project id"),
        ("issue_id" = i32, Path, description = "Issue id"),
    ),
    request_body = IssueSolvedInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 404, description = "No such project the caller may see, or issue on it", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn solve(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, issue_id)): Path<(i32, i32)>,
    JsonBody(input): JsonBody<IssueSolvedInput>,
) -> ApiResult<StatusCode> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    visible_project(&mut conn, &viewer, id).await?;
    require_issue(&mut conn, id, issue_id).await?;
    drop(conn);
    mark_issue_solved(
        &state.pool,
        &user,
        issue_id,
        input.solved,
        "id = ?",
        issue_id,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete an issue reported on the project
///
/// Only the project's own issues (a job's are deleted on the job), by their
/// reporter, an admin or a superuser; its pictures go from storage.
#[crate::openapi::path(
    delete,
    path = "/v1/projects/{id}/issues/{issue_id}",
    operation_id = "delete_project_issue",
    tag = "projects",
    params(
        ("id" = i32, Path, description = "Project id"),
        ("issue_id" = i32, Path, description = "Issue id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 404, description = "Not an issue on this project the caller may delete", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, issue_id)): Path<(i32, i32)>,
) -> ApiResult<StatusCode> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    visible_project(&mut conn, &viewer, id).await?;
    let any = user.is_admin || user.is_superuser;
    let deleted = sqlx::query(
        "UPDATE event_issues SET deleted_at = NOW()
          WHERE id = ? AND project_id = ? AND company_id = ? AND deleted_at IS NULL
            AND (? OR created_by = ?)",
    )
    .bind(issue_id)
    .bind(id)
    .bind(user.company_id)
    .bind(any)
    .bind(user.id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if deleted == 0 {
        return Err(ApiError::NotFound);
    }
    let urls: Vec<String> =
        sqlx::query_scalar("SELECT url FROM event_issue_images WHERE issue_id = ?")
            .bind(issue_id)
            .fetch_all(&mut *conn)
            .await?;
    for url in urls {
        remove(&state.storage, &url, user.company_id).await;
    }
    Ok(StatusCode::NO_CONTENT)
}
