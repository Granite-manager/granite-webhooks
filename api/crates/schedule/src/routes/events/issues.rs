//! Issues on an event: a problem the crew reports from the job (the app's
//! Issue tab), with a description and pictures. Filing one tells the job's
//! sales rep.
//!
//! - `POST /v1/events/{id}/issues`
//! - `PUT /v1/events/{id}/issues/{issue_id}` (mark solved, or open again)
//! - `DELETE /v1/events/{id}/issues/{issue_id}`
//!
//! `GET /v1/events/{id}/details` lists them (`issues`).

use crate::auth::ShopOrEmployeeUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::phone::crm_push::push_notification_logged;
use crate::routes::uploads::{self, schemas::UploadFolder};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use sqlx::{MySql, MySqlPool, QueryBuilder};
use std::collections::HashMap;

/// The longest description, in characters.
pub const MAX_DESCRIPTION_CHARS: usize = 2000;
/// The most pictures on one issue.
pub const MAX_PICTURES: usize = 20;
/// `notifications.notification_type` of the sales rep's notification.
pub const EVENT_ISSUE: &str = "event_issue";

/// A picture of an issue.
#[derive(Debug, Serialize, ToSchema)]
pub struct IssuePhoto {
    /// The picture's id.
    pub id: i32,
    /// Its public URL (a `-icon` thumbnail exists beside it).
    pub url: String,
}

/// A problem reported on an event.
#[derive(Debug, Serialize, ToSchema)]
pub struct EventIssue {
    /// The issue's id.
    pub id: i32,
    /// The event it is on.
    pub event_id: i32,
    /// What is wrong, as the reporter wrote it (1 to 2000 characters).
    pub description: String,
    /// When it was reported (UTC).
    pub created_at: Option<DateTime<Utc>>,
    /// The reporter's user id; null once that user is gone.
    pub created_by_id: Option<i32>,
    /// The reporter's name.
    pub created_by: Option<String>,
    /// When it was marked solved (UTC); null while it is open.
    pub solved_at: Option<DateTime<Utc>>,
    /// Who marked it solved.
    pub solved_by: Option<String>,
    /// Its pictures, in the order they were sent.
    pub photos: Vec<IssuePhoto>,
}

/// Body of `POST /v1/events/{id}/issues`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct IssueInput {
    /// What is wrong: 1 to 2000 characters (trimmed).
    pub description: String,
    /// Pictures of it: `url`s that `POST /v1/uploads` (folder `events`)
    /// returned, after each file was `PUT` to its `upload_url`. At most 20;
    /// may be empty.
    #[serde(default)]
    pub urls: Vec<String>,
}

#[derive(sqlx::FromRow)]
struct IssueRow {
    id: i32,
    event_id: i32,
    description: String,
    created_at: Option<DateTime<Utc>>,
    created_by_id: Option<i32>,
    created_by: Option<String>,
    solved_at: Option<DateTime<Utc>>,
    solved_by: Option<String>,
}

/// Body of `PUT /v1/events/{id}/issues/{issue_id}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct IssueSolved {
    /// Solved, or open again.
    pub solved: bool,
}

/// Mark an issue solved, or open again
///
/// Anyone who can see the job: the crew on the job card, the office on the
/// schedule. Solved keeps who did it first and when.
#[crate::openapi::path(
    put,
    path = "/v1/events/{id}/issues/{issue_id}",
    operation_id = "set_event_issue_solved",
    tag = "schedule",
    params(
        ("id" = i32, Path, description = "Event id"),
        ("issue_id" = i32, Path, description = "Issue id"),
    ),
    request_body = IssueSolved,
    responses(
        (status = 204, description = "Saved"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, admin or shop manager or worker", body = ErrorBody),
        (status = 404, description = "No such issue on this event in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn solve(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path((id, issue_id)): Path<(i32, i32)>,
    JsonBody(input): JsonBody<IssueSolved>,
) -> ApiResult<StatusCode> {
    if !crate::routes::projects::mark_issue_solved(
        &state.pool,
        &user,
        issue_id,
        input.solved,
        "event_id = ?",
        id,
    )
    .await?
    {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The event's issues, oldest first, each with its pictures.
pub async fn list(
    pool: &MySqlPool,
    company_id: i32,
    event_id: i32,
) -> sqlx::Result<Vec<EventIssue>> {
    let rows: Vec<IssueRow> = sqlx::query_as(
        "SELECT id, event_id, description, created_at, created_by AS created_by_id,
                created_by_name AS created_by, solved_at, solved_by_name AS solved_by
           FROM event_issues
          WHERE event_id = ? AND company_id = ? AND deleted_at IS NULL ORDER BY created_at, id",
    )
    .bind(event_id)
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<i32> = rows.iter().map(|row| row.id).collect();
    let mut query =
        QueryBuilder::<MySql>::new("SELECT id, issue_id, url FROM event_issue_images WHERE 1 = 1");
    crate::sql::push_in(&mut query, "issue_id", &ids);
    query.push(" ORDER BY id");
    let images: Vec<(i32, i32, String)> = query.build_query_as().fetch_all(pool).await?;
    let mut by_issue: HashMap<i32, Vec<IssuePhoto>> = HashMap::new();
    for (id, issue_id, url) in images {
        by_issue
            .entry(issue_id)
            .or_default()
            .push(IssuePhoto { id, url });
    }
    Ok(rows
        .into_iter()
        .map(|row| EventIssue {
            photos: by_issue.remove(&row.id).unwrap_or_default(),
            id: row.id,
            event_id: row.event_id,
            description: row.description,
            created_at: row.created_at,
            created_by_id: row.created_by_id,
            solved_at: row.solved_at,
            solved_by: row.solved_by,
            created_by: row.created_by,
        })
        .collect())
}

/// Report an issue on an event
///
/// Files a problem with the job (the app's Issue tab, for the crew and
/// anyone else on the schedule): a description and, optionally, pictures
/// uploaded to the `events` folder first. The job's sales rep (the sale's
/// seller, else the deal's owner, else the customer's sales rep, else
/// whoever made the event) gets a bell notification of type `event_issue`
/// and a phone push, unless they reported it themselves. `GET
/// /v1/events/{id}/details` lists the event's issues.
#[crate::openapi::path(
    post,
    path = "/v1/events/{id}/issues",
    operation_id = "report_event_issue",
    tag = "schedule",
    params(("id" = i32, Path, description = "Event id")),
    request_body = IssueInput,
    responses(
        (status = 201, description = "Reported"),
        (status = 400, description = "No description, one over 2000 characters, over 20 pictures, or a URL that was not uploaded", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, admin or shop manager or worker", body = ErrorBody),
        (status = 404, description = "No such event in the caller's company", body = ErrorBody),
        (status = 500, description = "Database or storage error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn report(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<IssueInput>,
) -> ApiResult<StatusCode> {
    let pool = &state.pool;
    super::details::event_exists(pool, user.company_id, id).await?;
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
        uploads::attach(&state.storage, url, UploadFolder::Events, user.company_id).await?;
    }
    let mut tx = pool.begin().await?;
    let issue_id = sqlx::query(
        "INSERT INTO event_issues (event_id, company_id, description, created_by, created_by_name)
         VALUES (?, ?, ?, ?, ?)",
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
    // The issue is filed either way: a rep who could not be told still sees it on the event.
    if let Err(error) = tell_rep(&state, id, description, &user).await {
        tracing::error!(
            event_id = id,
            "event issue: could not tell the rep: {error}"
        );
    }
    Ok(StatusCode::CREATED)
}

/// Delete an issue I reported
///
/// Only the reporter, an admin or a superuser can delete it; its pictures
/// go from storage. The sales rep's notification stays.
#[crate::openapi::path(
    delete,
    path = "/v1/events/{id}/issues/{issue_id}",
    operation_id = "delete_event_issue",
    tag = "schedule",
    params(
        ("id" = i32, Path, description = "Event id"),
        ("issue_id" = i32, Path, description = "Issue id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, admin or shop manager or worker", body = ErrorBody),
        (status = 404, description = "Not an issue on this event the caller may delete", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path((id, issue_id)): Path<(i32, i32)>,
) -> ApiResult<StatusCode> {
    let pool = &state.pool;
    let any = user.is_admin || user.is_superuser;
    let deleted = sqlx::query(
        "UPDATE event_issues SET deleted_at = NOW()
          WHERE id = ? AND event_id = ? AND company_id = ? AND deleted_at IS NULL
            AND (? OR created_by = ?)",
    )
    .bind(issue_id)
    .bind(id)
    .bind(user.company_id)
    .bind(any)
    .bind(user.id)
    .execute(pool)
    .await?
    .rows_affected();
    if deleted == 0 {
        return Err(ApiError::NotFound);
    }
    let urls: Vec<String> =
        sqlx::query_scalar("SELECT url FROM event_issue_images WHERE issue_id = ?")
            .bind(issue_id)
            .fetch_all(pool)
            .await?;
    for url in urls {
        uploads::remove(&state.storage, &url, user.company_id).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Who hears about an issue, and what it is on.
#[derive(sqlx::FromRow)]
struct IssueJob {
    title: Option<String>,
    rep_id: Option<i32>,
    customer_id: Option<i32>,
    customer_name: Option<String>,
    deal_id: Option<u64>,
}

/// An issue was just reported: the job's sales rep (the sale's seller, else
/// the deal's owner, else the customer's sales rep, else whoever made the
/// event) gets a bell notification and a phone push, unless they reported
/// it themselves.
async fn tell_rep(
    state: &AppState,
    event_id: i32,
    description: &str,
    by: &SessionUser,
) -> Result<(), sqlx::Error> {
    let pool = &state.pool;
    let job: Option<IssueJob> = sqlx::query_as(
        "SELECT NULLIF(TRIM(e.title), '') AS title,
                CAST(COALESCE(sr.id, dr.id, cr.id, mk.id) AS SIGNED) AS rep_id,
                CAST(c.id AS SIGNED) AS customer_id,
                NULLIF(TRIM(c.name), '') AS customer_name,
                CAST(d.id AS UNSIGNED) AS deal_id
           FROM events e
           LEFT JOIN sales s ON s.id = e.sale_id AND s.company_id = ?
           LEFT JOIN customers c ON c.id = COALESCE(e.customer_id, s.customer_id)
            AND c.company_id = ? AND c.deleted_at IS NULL
           LEFT JOIN deals d ON d.id = s.deal_id AND d.deleted_at IS NULL
           LEFT JOIN users sr ON sr.id = s.seller_id AND sr.company_id = ? AND sr.is_deleted = 0
           LEFT JOIN users dr ON dr.id = d.user_id AND dr.company_id = ? AND dr.is_deleted = 0
           LEFT JOIN users cr ON cr.id = c.sales_rep AND cr.company_id = ? AND cr.is_deleted = 0
           LEFT JOIN users mk ON mk.id = e.created_user_id AND mk.company_id = ?
            AND mk.is_deleted = 0
          WHERE e.id = ?",
    )
    .bind(by.company_id)
    .bind(by.company_id)
    .bind(by.company_id)
    .bind(by.company_id)
    .bind(by.company_id)
    .bind(by.company_id)
    .bind(event_id)
    .fetch_optional(pool)
    .await?;
    let Some(job) = job else {
        return Ok(());
    };
    let Some(rep_id) = job.rep_id.filter(|rep| *rep != by.id) else {
        return Ok(());
    };
    let about = job
        .customer_name
        .as_deref()
        .or(job.title.as_deref())
        .unwrap_or("a job");
    let message: String = format!("Issue on {about}: {description}")
        .chars()
        .take(255)
        .collect();
    let notification_id = sqlx::query(
        "INSERT INTO notifications
                (user_id, customer_id, deal_id, event_id, message, notification_type,
                 actor_name, due_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, NOW())",
    )
    .bind(rep_id)
    .bind(job.customer_id)
    .bind(job.deal_id)
    .bind(event_id)
    .bind(&message)
    .bind(EVENT_ISSUE)
    .bind(by.name.as_deref())
    .execute(pool)
    .await?
    .last_insert_id();
    push_notification_logged(state, i64::try_from(notification_id).unwrap_or_default()).await;
    Ok(())
}
