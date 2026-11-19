//! Which project a schedule event is for, picked by hand in Event details.
//!
//! A project takes the events of its deal's quotes when it starts, and the
//! customer's other events made since the deal began when the customer has no
//! other project. An event made another way (by hand, long before the
//! project) is linked here, together with the customer's other events
//! (template and installation), so the project's steps change them.

use super::access::Viewer;
use super::service::visible_project;
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, MySql, MySqlConnection, QueryBuilder};

/// A project the event can be linked to.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct LinkProject {
    pub id: i32,
    pub title: String,
    /// Its stage code (`new`, `templated`, …).
    pub stage_code: String,
}

/// One of the customer's events.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct LinkEvent {
    pub id: i32,
    pub title: String,
    /// UTC, `YYYY-MM-DDTHH:MM:SSZ`; null for an unscheduled event.
    pub start_date: Option<String>,
    pub all_day: bool,
    /// Its calendar's name.
    pub calendar_name: String,
    /// The project it is linked to.
    pub project_id: Option<i32>,
}

/// `GET /v1/projects/event-links/{event_id}`.
#[derive(Debug, Serialize, ToSchema)]
pub struct EventProjectLink {
    /// The project the event is linked to.
    pub project_id: Option<i32>,
    /// The customer's open projects the caller can see.
    pub projects: Vec<LinkProject>,
    /// The customer's events (this one included), soonest first; only this
    /// one when the event has no customer.
    pub events: Vec<LinkEvent>,
}

/// `PUT /v1/projects/event-links/{event_id}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct EventProjectLinkInput {
    /// The project; null unlinks.
    pub project_id: Option<i32>,
    /// The customer's other events to link (or unlink) with it.
    #[serde(default)]
    pub event_ids: Vec<i32>,
}

/// The event's customer and project; 404 for another company's or a deleted event.
async fn event_row(
    conn: &mut MySqlConnection,
    company_id: i32,
    event_id: i32,
) -> ApiResult<(Option<i32>, Option<i32>)> {
    sqlx::query_as(
        "SELECT e.customer_id, e.project_id FROM events e
           JOIN calendars cal ON cal.id = e.calendar_id AND cal.company_id = ?
          WHERE e.id = ? AND e.deleted_date IS NULL",
    )
    .bind(company_id)
    .bind(event_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(ApiError::NotFound)
}

/// A project the caller may change.
async fn editable(conn: &mut MySqlConnection, viewer: &Viewer, id: i32) -> ApiResult<i32> {
    let perms = viewer.permissions;
    Viewer::require(perms.production || perms.manage || perms.convert)?;
    let project = visible_project(conn, viewer, id).await?;
    if project.archived_at.is_some()
        || project.cancelled_at.is_some()
        || project.sale_cancelled_at.is_some()
    {
        return Err(ApiError::Conflict(
            "This project is archived or cancelled, so its events can't change.".into(),
        ));
    }
    Ok(project.customer_id)
}

/// The project an event is for, and what it can be linked to
#[crate::openapi::path(
    get,
    path = "/v1/projects/event-links/{event_id}",
    operation_id = "get_event_project_link",
    tag = "projects",
    params(("event_id" = i32, Path, description = "Event id")),
    responses(
        (status = 200, description = "The link and the choices", body = EventProjectLink),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 404, description = "No such event in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(event_id): Path<i32>,
) -> ApiResult<Json<EventProjectLink>> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let company = viewer.company_id;
    let mut conn = state.pool.acquire().await?;
    let (customer_id, project_id) = event_row(&mut conn, company, event_id).await?;
    let projects: Vec<LinkProject> = match customer_id {
        Some(customer) => {
            let sql = format!(
                "SELECT p.id, p.title, CAST(p.stage_code AS CHAR) AS stage_code FROM projects p
                  WHERE p.company_id = ? AND p.customer_id = ?
                    AND p.archived_at IS NULL AND p.cancelled_at IS NULL AND {}
                  ORDER BY p.id DESC",
                viewer.visible_sql()
            );
            sqlx::query_as(sqlx::AssertSqlSafe(sql))
                .bind(company)
                .bind(customer)
                .fetch_all(&mut *conn)
                .await?
        }
        None => Vec::new(),
    };
    let mut sql = QueryBuilder::<MySql>::new(
        "SELECT e.id, e.title,
                IF(COALESCE(e.unscheduled, 0) <> 0, NULL,
                   DATE_FORMAT(e.start_date, '%Y-%m-%dT%H:%i:%sZ')) AS start_date,
                COALESCE(e.all_day, 0) <> 0 AS all_day,
                cal.name AS calendar_name, e.project_id
           FROM events e
           JOIN calendars cal ON cal.id = e.calendar_id AND cal.deleted_at IS NULL
                             AND cal.company_id = ",
    );
    sql.push_bind(company);
    sql.push(" WHERE e.deleted_date IS NULL AND ");
    match customer_id {
        Some(customer) => sql.push("e.customer_id = ").push_bind(customer),
        None => sql.push("e.id = ").push_bind(event_id),
    };
    sql.push(" ORDER BY COALESCE(e.unscheduled, 0) <> 0, e.start_date, e.id");
    let events: Vec<LinkEvent> = sql.build_query_as().fetch_all(&mut *conn).await?;
    Ok(Json(EventProjectLink {
        project_id,
        projects,
        events,
    }))
}

/// Link an event, and the customer's other events picked, to a project
///
/// Null unlinks them. The events must be this event's customer's; each one
/// leaves the part of its old project.
#[crate::openapi::path(
    put,
    path = "/v1/projects/event-links/{event_id}",
    operation_id = "set_event_project_link",
    tag = "projects",
    params(("event_id" = i32, Path, description = "Event id")),
    request_body = EventProjectLinkInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "An event that is not the customer's", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "The caller's position can't change projects", body = ErrorBody),
        (status = 404, description = "No such event, or project the caller may see", body = ErrorBody),
        (status = 409, description = "The project is archived or cancelled", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(event_id): Path<i32>,
    JsonBody(input): JsonBody<EventProjectLinkInput>,
) -> ApiResult<StatusCode> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let company = viewer.company_id;
    let mut conn = state.pool.acquire().await?;
    let (customer_id, _) = event_row(&mut conn, company, event_id).await?;
    let mut ids = vec![event_id];
    for id in input.event_ids {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    // Every event: the company's, not deleted, this event's customer's.
    let mut sql = QueryBuilder::<MySql>::new(
        "SELECT e.id, e.customer_id, e.project_id FROM events e
           JOIN calendars cal ON cal.id = e.calendar_id AND cal.company_id = ",
    );
    sql.push_bind(company);
    sql.push(" WHERE e.deleted_date IS NULL");
    crate::sql::push_in(&mut sql, "e.id", &ids);
    let rows: Vec<(i32, Option<i32>, Option<i32>)> =
        sql.build_query_as().fetch_all(&mut *conn).await?;
    if rows.len() != ids.len()
        || rows.iter().any(|(id, customer, _)| {
            *id != event_id && customer_id.is_none_or(|c| *customer != Some(c))
        })
    {
        return Err(ApiError::BadRequest(
            "Pick only this customer's events".into(),
        ));
    }
    let target = match input.project_id.filter(|id| *id > 0) {
        Some(project) => {
            let owner = editable(&mut conn, &viewer, project).await?;
            if customer_id.is_some_and(|c| c != owner) {
                return Err(ApiError::BadRequest(
                    "Pick one of this customer's projects".into(),
                ));
            }
            Some(project)
        }
        None => None,
    };
    // The projects the events leave must be the caller's to change too.
    let mut touched: Vec<i32> = target.into_iter().collect();
    for (_, _, old) in &rows {
        if let Some(old) = old
            && Some(*old) != target
            && !touched.contains(old)
        {
            editable(&mut conn, &viewer, *old).await?;
            touched.push(*old);
        }
    }
    let mut sql =
        QueryBuilder::<MySql>::new("UPDATE events SET project_part_id = IF(project_id <=> ");
    sql.push_bind(target);
    sql.push(", project_part_id, NULL), project_id = ");
    sql.push_bind(target);
    sql.push(" WHERE deleted_date IS NULL");
    crate::sql::push_in(&mut sql, "id", &ids);
    sql.build().execute(&mut *conn).await?;
    for project in touched {
        sqlx::query("UPDATE projects SET version = version + 1 WHERE id = ? AND company_id = ?")
            .bind(project)
            .bind(company)
            .execute(&mut *conn)
            .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}
