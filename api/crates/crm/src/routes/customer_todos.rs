//! Customer to-dos: what the customer must do before install.
//!
//! Put in overhang supports, level the cabinets, bring the sink to the shop.
//! Admins keep the
//! lists in Admin > Schedule; the template maker ticks what applies at the
//! template appointment and sends it; the job's sales rep is told, and the
//! to-dos show on the customer, the deal, the project and the appointment,
//! where anyone in the company ticks them off as the customer does them.
//!
//! - `GET    /v1/customer-checklists`              → the company's lists (anyone)
//! - `POST   /v1/customer-checklists`              → add a list (admins)
//! - `PUT    /v1/customer-checklists/{id}`         → rename it, set its to-dos (admins)
//! - `DELETE /v1/customer-checklists/{id}`         → delete it (admins)
//! - `POST   /v1/events/{id}/customer-todos`       → send a template's to-dos, or nothing to do
//! - `PUT    /v1/customer-todos/{id}`              → done, or not done again
//! - `DELETE /v1/customer-todos/{id}`              → the sender or an admin
//! - `GET    /v1/customers/{id}/customer-todos`    → a customer's to-dos
//!
//! `GET /v1/events/{id}/details` (`customer_todos`) and `GET /v1/projects/{id}`
//! (`customer_todos`) list them too.

use crate::auth::session::SessionUser;
use crate::auth::{AdminUser, EmployeeUser, ShopOrEmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::{OpenApiRouter, ToSchema, routes};
use crate::phone::crm_push::push_notification_logged;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use sqlx::{MySqlConnection, MySqlPool};

/// The longest list name, in characters.
const MAX_NAME_CHARS: usize = 120;
/// The longest to-do, in characters.
const MAX_LABEL_CHARS: usize = 200;
/// The longest note on a to-do, in characters.
const MAX_NOTE_CHARS: usize = 1000;
/// The most to-dos on one list, or sent at once.
const MAX_ITEMS: usize = 60;
/// `notifications.notification_type` of the sales rep's notification.
pub const CUSTOMER_TODOS: &str = "customer_todos";

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_checklists, create_checklist))
        .routes(routes!(update_checklist, delete_checklist))
        .routes(routes!(send_event_todos))
        .routes(routes!(set_todo_done, delete_todo))
        .routes(routes!(customer_todos))
}

/// One to-do on a list.
#[derive(Debug, Serialize, ToSchema)]
pub struct CustomerChecklistItem {
    /// The to-do's id.
    pub id: i32,
    /// What the customer must do, e.g. "Level the cabinets".
    pub label: String,
}

/// A list of to-dos the template maker picks from.
#[derive(Debug, Serialize, ToSchema)]
pub struct CustomerChecklist {
    /// The list's id.
    pub id: i32,
    /// Its name, e.g. "Before install".
    pub name: String,
    /// Its to-dos, in order.
    pub items: Vec<CustomerChecklistItem>,
}

/// Body of `POST /v1/customer-checklists` and `PUT /v1/customer-checklists/{id}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CustomerChecklistInput {
    /// The name: 1 to 120 characters (trimmed).
    pub name: String,
    /// The to-dos, in order: each 1 to 200 characters (trimmed; blank ones
    /// are dropped), at most 60. Saving replaces the list's to-dos; to-dos
    /// already sent to customers stay as they are.
    #[serde(default)]
    pub items: Vec<String>,
}

/// One to-do of one customer.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct CustomerTodo {
    /// The to-do's id.
    pub id: i32,
    /// The customer who must do it.
    pub customer_id: i32,
    /// The template appointment it came from; null once that is deleted.
    pub event_id: Option<i32>,
    /// The project it is for; null when the customer had no open project.
    pub project_id: Option<i32>,
    /// What the customer must do.
    pub label: String,
    /// The template maker's note, e.g. "Island, 2 corbels on the seating side".
    pub note: Option<String>,
    /// When it was sent (UTC).
    pub created_at: Option<DateTime<Utc>>,
    /// The sender's user id; null once that user is gone.
    pub created_by_id: Option<i32>,
    /// The sender's name.
    pub created_by: Option<String>,
    /// When it was ticked off (UTC); null while the customer still has to do it.
    pub done_at: Option<DateTime<Utc>>,
    /// Who ticked it off.
    pub done_by: Option<String>,
}

/// A template appointment's to-dos.
#[derive(Debug, Serialize, ToSchema)]
pub struct EventCustomerTodos {
    /// When the template maker sent them (UTC); null until then.
    pub sent_at: Option<DateTime<Utc>>,
    /// Who sent them.
    pub sent_by: Option<String>,
    /// True when they said there is nothing to do.
    pub nothing_to_do: bool,
    /// The to-dos sent from this appointment, in the order they were sent.
    pub todos: Vec<CustomerTodo>,
}

/// One to-do sent from a template appointment.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CustomerTodoInput {
    /// What the customer must do: 1 to 200 characters (trimmed).
    pub label: String,
    /// A note for the sales rep: at most 1000 characters; blank means none.
    #[serde(default)]
    pub note: Option<String>,
}

/// Body of `POST /v1/events/{id}/customer-todos`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SendCustomerTodosInput {
    /// The list they were picked from (`GET /v1/customer-checklists`), if any.
    #[serde(default)]
    pub checklist_id: Option<i32>,
    /// The to-dos that apply to this customer, at most 60. Empty means there
    /// is nothing to do.
    #[serde(default)]
    pub items: Vec<CustomerTodoInput>,
}

/// Body of `PUT /v1/customer-todos/{id}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CustomerTodoDoneInput {
    /// Done, or not done again.
    pub done: bool,
}

const TODO_SELECT: &str = "SELECT t.id, t.customer_id, t.event_id, t.project_id, t.label, t.note,
        t.created_at, t.created_by AS created_by_id, t.created_by_name AS created_by,
        t.done_at, t.done_by_name AS done_by
   FROM customer_todos t";

fn trimmed_name(name: &str) -> ApiResult<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest("Name the checklist".into()));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(ApiError::BadRequest(
            "The name must be 120 characters or less".into(),
        ));
    }
    Ok(name.to_string())
}

fn trimmed_label(label: &str) -> ApiResult<Option<String>> {
    let label = label.trim();
    if label.is_empty() {
        return Ok(None);
    }
    if label.chars().count() > MAX_LABEL_CHARS {
        return Err(ApiError::BadRequest(
            "Each to-do must be 200 characters or less".into(),
        ));
    }
    Ok(Some(label.to_string()))
}

fn trimmed_items(items: &[String]) -> ApiResult<Vec<String>> {
    let mut out = Vec::new();
    for item in items {
        if let Some(label) = trimmed_label(item)? {
            out.push(label);
        }
    }
    if out.len() > MAX_ITEMS {
        return Err(ApiError::BadRequest("At most 60 to-dos".into()));
    }
    Ok(out)
}

async fn checklists(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Vec<CustomerChecklist>> {
    let lists: Vec<(i32, String)> = sqlx::query_as(
        "SELECT id, name FROM customer_checklists
          WHERE company_id = ? AND deleted_at IS NULL ORDER BY sort_order, id",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    let items: Vec<(i32, i32, String)> = sqlx::query_as(
        "SELECT i.id, i.checklist_id, i.label FROM customer_checklist_items i
           JOIN customer_checklists l ON l.id = i.checklist_id
          WHERE l.company_id = ? AND l.deleted_at IS NULL ORDER BY i.sort_order, i.id",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    Ok(lists
        .into_iter()
        .map(|(id, name)| CustomerChecklist {
            items: items
                .iter()
                .filter(|(_, list, _)| *list == id)
                .map(|(item_id, _, label)| CustomerChecklistItem {
                    id: *item_id,
                    label: label.clone(),
                })
                .collect(),
            id,
            name,
        })
        .collect())
}

async fn write_items(
    conn: &mut MySqlConnection,
    checklist_id: i32,
    items: &[String],
) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM customer_checklist_items WHERE checklist_id = ?")
        .bind(checklist_id)
        .execute(&mut *conn)
        .await?;
    for (index, label) in items.iter().enumerate() {
        sqlx::query(
            "INSERT INTO customer_checklist_items (checklist_id, label, sort_order) VALUES (?, ?, ?)",
        )
        .bind(checklist_id)
        .bind(label)
        .bind(i32::try_from(index).unwrap_or(i32::MAX))
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// List the company's customer checklists
///
/// The lists of to-dos a template maker picks from at a template appointment
/// (what the customer must do before install), in order, each with its
/// to-dos. Anyone in the company. Send the picked ones with `POST
/// /v1/events/{id}/customer-todos`.
#[crate::openapi::path(
    get,
    path = "/v1/customer-checklists",
    operation_id = "list_customer_checklists",
    tag = "schedule",
    responses(
        (status = 200, description = "The lists", body = Vec<CustomerChecklist>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, admin or shop manager or worker", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_checklists(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
) -> ApiResult<Json<Vec<CustomerChecklist>>> {
    Ok(Json(checklists(&state.pool, user.company_id).await?))
}

/// Add a customer checklist
///
/// A named list of to-dos the customer may need to do before install. Admins
/// only. It goes last. Returns every list, the new one included.
#[crate::openapi::path(
    post,
    path = "/v1/customer-checklists",
    operation_id = "create_customer_checklist",
    tag = "schedule",
    request_body = CustomerChecklistInput,
    responses(
        (status = 201, description = "Added; every list", body = Vec<CustomerChecklist>),
        (status = 400, description = "No name, one over 120 characters, a to-do over 200 characters, or over 60 to-dos", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_checklist(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<CustomerChecklistInput>,
) -> ApiResult<(StatusCode, Json<Vec<CustomerChecklist>>)> {
    let name = trimmed_name(&input.name)?;
    let items = trimmed_items(&input.items)?;
    let mut tx = state.pool.begin().await?;
    let next: Option<i32> = sqlx::query_scalar(
        "SELECT CAST(MAX(sort_order) + 1 AS SIGNED) FROM customer_checklists
          WHERE company_id = ? AND deleted_at IS NULL",
    )
    .bind(user.company_id)
    .fetch_one(&mut *tx)
    .await?;
    let id = sqlx::query(
        "INSERT INTO customer_checklists (company_id, name, sort_order) VALUES (?, ?, ?)",
    )
    .bind(user.company_id)
    .bind(&name)
    .bind(next.unwrap_or(0))
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    write_items(&mut tx, i32::try_from(id).unwrap_or_default(), &items).await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(checklists(&state.pool, user.company_id).await?),
    ))
}

/// Rename a customer checklist and set its to-dos
///
/// Replaces the name and the to-dos, in the order given. To-dos already sent
/// to customers stay as they were. Admins only. Returns every list.
#[crate::openapi::path(
    put,
    path = "/v1/customer-checklists/{id}",
    operation_id = "update_customer_checklist",
    tag = "schedule",
    params(("id" = i32, Path, description = "Checklist id")),
    request_body = CustomerChecklistInput,
    responses(
        (status = 200, description = "Saved; every list", body = Vec<CustomerChecklist>),
        (status = 400, description = "No name, one over 120 characters, a to-do over 200 characters, or over 60 to-dos", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such list in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_checklist(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<CustomerChecklistInput>,
) -> ApiResult<Json<Vec<CustomerChecklist>>> {
    let name = trimmed_name(&input.name)?;
    let items = trimmed_items(&input.items)?;
    let mut tx = state.pool.begin().await?;
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM customer_checklists
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(&mut *tx)
    .await?;
    if found.is_none() {
        return Err(ApiError::NotFound);
    }
    sqlx::query("UPDATE customer_checklists SET name = ? WHERE id = ?")
        .bind(&name)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    write_items(&mut tx, id, &items).await?;
    tx.commit().await?;
    Ok(Json(checklists(&state.pool, user.company_id).await?))
}

/// Delete a customer checklist
///
/// Template makers no longer see it. To-dos already sent to customers stay.
/// Admins only.
#[crate::openapi::path(
    delete,
    path = "/v1/customer-checklists/{id}",
    operation_id = "delete_customer_checklist",
    tag = "schedule",
    params(("id" = i32, Path, description = "Checklist id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such list in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_checklist(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    let deleted = sqlx::query(
        "UPDATE customer_checklists SET deleted_at = NOW()
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?
    .rows_affected();
    if deleted == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The appointment's customer, project and sales rep.
#[derive(sqlx::FromRow)]
struct TodoJob {
    customer_id: Option<i32>,
    customer_name: Option<String>,
    project_id: Option<i32>,
    rep_id: Option<i32>,
    deal_id: Option<u64>,
}

async fn todo_job(pool: &MySqlPool, company_id: i32, event_id: i32) -> ApiResult<TodoJob> {
    let job: Option<TodoJob> = sqlx::query_as(
        "SELECT CAST(c.id AS SIGNED) AS customer_id,
                NULLIF(TRIM(c.name), '') AS customer_name,
                CAST(COALESCE(ep.id, (SELECT p.id FROM projects p
                                       WHERE p.company_id = ? AND p.customer_id = c.id
                                         AND p.archived_at IS NULL AND p.cancelled_at IS NULL
                                       ORDER BY p.created_at DESC, p.id DESC LIMIT 1)) AS SIGNED)
                  AS project_id,
                CAST(COALESCE(sr.id, dr.id, pr.id, cr.id, mk.id) AS SIGNED) AS rep_id,
                CAST(d.id AS UNSIGNED) AS deal_id
           FROM events e
           JOIN users mk0 ON mk0.id = e.created_user_id AND mk0.company_id = ?
           LEFT JOIN sales s ON s.id = e.sale_id AND s.company_id = ?
           LEFT JOIN customers c ON c.id = COALESCE(e.customer_id, s.customer_id)
            AND c.company_id = ? AND c.deleted_at IS NULL
           LEFT JOIN projects ep ON ep.id = e.project_id AND ep.company_id = ?
           LEFT JOIN deals d ON d.id = s.deal_id AND d.deleted_at IS NULL
           LEFT JOIN users sr ON sr.id = s.seller_id AND sr.company_id = ? AND sr.is_deleted = 0
           LEFT JOIN users dr ON dr.id = d.user_id AND dr.company_id = ? AND dr.is_deleted = 0
           LEFT JOIN users pr ON pr.id = ep.sales_rep_id AND pr.company_id = ? AND pr.is_deleted = 0
           LEFT JOIN users cr ON cr.id = c.sales_rep AND cr.company_id = ? AND cr.is_deleted = 0
           LEFT JOIN users mk ON mk.id = e.created_user_id AND mk.is_deleted = 0
          WHERE e.id = ? AND e.deleted_date IS NULL",
    )
    .bind(company_id)
    .bind(company_id)
    .bind(company_id)
    .bind(company_id)
    .bind(company_id)
    .bind(company_id)
    .bind(company_id)
    .bind(company_id)
    .bind(company_id)
    .bind(event_id)
    .fetch_optional(pool)
    .await?;
    job.ok_or(ApiError::NotFound)
}

/// The to-dos of a template appointment, for `GET /v1/events/{id}/details`.
pub async fn event_todos(
    pool: &MySqlPool,
    company_id: i32,
    event_id: i32,
) -> sqlx::Result<EventCustomerTodos> {
    let report: Option<(Option<DateTime<Utc>>, Option<String>, i32)> = sqlx::query_as(
        "SELECT created_at, created_by_name, todo_count FROM customer_todo_reports
          WHERE event_id = ? AND company_id = ?",
    )
    .bind(event_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let todos: Vec<CustomerTodo> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{TODO_SELECT} WHERE t.event_id = ? AND t.company_id = ? AND t.deleted_at IS NULL
          ORDER BY t.created_at, t.id"
    )))
    .bind(event_id)
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    Ok(match report {
        Some((sent_at, sent_by, _)) => EventCustomerTodos {
            sent_at,
            sent_by,
            nothing_to_do: todos.is_empty(),
            todos,
        },
        None => EventCustomerTodos {
            sent_at: None,
            sent_by: None,
            nothing_to_do: false,
            todos,
        },
    })
}

/// The to-dos of a project, for `GET /v1/projects/{id}`: the ones sent for
/// it, and the ones from its appointments. Open ones first.
pub async fn project_todos(
    conn: &mut MySqlConnection,
    company_id: i32,
    project_id: i32,
) -> sqlx::Result<Vec<CustomerTodo>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{TODO_SELECT}
           JOIN projects p ON p.id = ? AND p.company_id = t.company_id
          WHERE t.company_id = ? AND t.deleted_at IS NULL
            AND (t.project_id = p.id
                 OR EXISTS (SELECT 1 FROM events e WHERE e.id = t.event_id
                             AND e.deleted_date IS NULL AND e.project_id = p.id))
          ORDER BY t.done_at IS NOT NULL, t.created_at, t.id"
    )))
    .bind(project_id)
    .bind(company_id)
    .fetch_all(&mut *conn)
    .await
}

/// Send a template appointment's customer to-dos
///
/// What the customer must do before install, picked at the template
/// appointment (from `GET /v1/customer-checklists`, or written in), each
/// with an optional note. Empty `items` means there is nothing to do.
/// Sending again adds to what was sent. The to-dos go on the appointment's
/// customer and their open project, and the job's sales rep (the sale's
/// seller, else the deal's owner, else the project's sales rep, else the
/// customer's sales rep, else whoever made the event) gets a bell
/// notification of type `customer_todos` and a phone push, unless they sent
/// them. Returns the appointment's to-dos.
#[crate::openapi::path(
    post,
    path = "/v1/events/{id}/customer-todos",
    operation_id = "send_event_customer_todos",
    tag = "schedule",
    params(("id" = i32, Path, description = "Event id")),
    request_body = SendCustomerTodosInput,
    responses(
        (status = 201, description = "Sent; the appointment's to-dos", body = EventCustomerTodos),
        (status = 400, description = "A to-do over 200 characters, a note over 1000, or over 60 to-dos", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, admin or shop manager or worker", body = ErrorBody),
        (status = 404, description = "No such event in the caller's company", body = ErrorBody),
        (status = 409, description = "The appointment has no customer", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn send_event_todos(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<SendCustomerTodosInput>,
) -> ApiResult<(StatusCode, Json<EventCustomerTodos>)> {
    let pool = &state.pool;
    if input.items.len() > MAX_ITEMS {
        return Err(ApiError::BadRequest("At most 60 to-dos".into()));
    }
    let mut items = Vec::new();
    for item in &input.items {
        let Some(label) = trimmed_label(&item.label)? else {
            continue;
        };
        let note = item
            .note
            .as_deref()
            .map(str::trim)
            .filter(|note| !note.is_empty());
        if note.is_some_and(|note| note.chars().count() > MAX_NOTE_CHARS) {
            return Err(ApiError::BadRequest(
                "Each note must be 1000 characters or less".into(),
            ));
        }
        items.push((label, note.map(str::to_string)));
    }
    let job = todo_job(pool, user.company_id, id).await?;
    let Some(customer_id) = job.customer_id else {
        return Err(ApiError::Conflict(
            "This appointment has no customer. Add the customer to it first.".into(),
        ));
    };
    let checklist_id: Option<i32> = match input.checklist_id {
        Some(list) => {
            sqlx::query_scalar("SELECT id FROM customer_checklists WHERE id = ? AND company_id = ?")
                .bind(list)
                .bind(user.company_id)
                .fetch_optional(pool)
                .await?
        }
        None => None,
    };
    let mut tx = pool.begin().await?;
    for (label, note) in &items {
        sqlx::query(
            "INSERT INTO customer_todos
                    (company_id, customer_id, event_id, project_id, checklist_id, label, note,
                     created_by, created_by_name)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(user.company_id)
        .bind(customer_id)
        .bind(id)
        .bind(job.project_id)
        .bind(checklist_id)
        .bind(label)
        .bind(note)
        .bind(user.id)
        .bind(&user.name)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "INSERT INTO customer_todo_reports (event_id, company_id, todo_count, created_by, created_by_name)
         VALUES (?, ?, ?, ?, ?)
         ON DUPLICATE KEY UPDATE todo_count = todo_count + VALUES(todo_count),
                                 created_by = VALUES(created_by),
                                 created_by_name = VALUES(created_by_name),
                                 created_at = CURRENT_TIMESTAMP",
    )
    .bind(id)
    .bind(user.company_id)
    .bind(i32::try_from(items.len()).unwrap_or(i32::MAX))
    .bind(user.id)
    .bind(&user.name)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    if !items.is_empty()
        && let Err(error) = tell_rep(&state, id, &job, customer_id, items.len(), &user).await
    {
        // The to-dos are saved either way: the rep still sees them on the deal.
        tracing::error!(event_id = id, "customer to-dos: could not tell the rep: {error}");
    }
    Ok((
        StatusCode::CREATED,
        Json(event_todos(pool, user.company_id, id).await?),
    ))
}

/// The to-dos were just sent: the job's sales rep gets a bell notification
/// and a phone push, unless they sent them.
async fn tell_rep(
    state: &AppState,
    event_id: i32,
    job: &TodoJob,
    customer_id: i32,
    count: usize,
    by: &SessionUser,
) -> Result<(), sqlx::Error> {
    let Some(rep_id) = job.rep_id.filter(|rep| *rep != by.id) else {
        return Ok(());
    };
    let about = job.customer_name.as_deref().unwrap_or("A customer");
    let message: String = if count == 1 {
        format!("{about} has 1 to-do from the template")
    } else {
        format!("{about} has {count} to-dos from the template")
    }
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
    .bind(customer_id)
    .bind(job.deal_id)
    .bind(event_id)
    .bind(&message)
    .bind(CUSTOMER_TODOS)
    .bind(by.name.as_deref())
    .execute(&state.pool)
    .await?
    .last_insert_id();
    push_notification_logged(state, i64::try_from(notification_id).unwrap_or_default()).await;
    Ok(())
}

/// Tick a customer to-do off, or mark it not done again
///
/// Anyone in the company: the sales rep on the deal, the office on the
/// project, the template maker on the appointment. Done keeps who did it
/// first and when.
#[crate::openapi::path(
    put,
    path = "/v1/customer-todos/{id}",
    operation_id = "set_customer_todo_done",
    tag = "schedule",
    params(("id" = i32, Path, description = "To-do id")),
    request_body = CustomerTodoDoneInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, admin or shop manager or worker", body = ErrorBody),
        (status = 404, description = "No such to-do in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_todo_done(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<CustomerTodoDoneInput>,
) -> ApiResult<StatusCode> {
    let pool = &state.pool;
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM customer_todos WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(pool)
    .await?;
    if found.is_none() {
        return Err(ApiError::NotFound);
    }
    if input.done {
        sqlx::query(
            "UPDATE customer_todos SET done_at = NOW(), done_by = ?, done_by_name = ?
              WHERE id = ? AND done_at IS NULL",
        )
        .bind(user.id)
        .bind(&user.name)
        .bind(id)
        .execute(pool)
        .await?;
    } else {
        sqlx::query(
            "UPDATE customer_todos SET done_at = NULL, done_by = NULL, done_by_name = NULL
              WHERE id = ?",
        )
        .bind(id)
        .execute(pool)
        .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a customer to-do
///
/// For one sent by mistake. Only whoever sent it, an admin or a superuser.
#[crate::openapi::path(
    delete,
    path = "/v1/customer-todos/{id}",
    operation_id = "delete_customer_todo",
    tag = "schedule",
    params(("id" = i32, Path, description = "To-do id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, admin or shop manager or worker", body = ErrorBody),
        (status = 404, description = "Not a to-do the caller may delete", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_todo(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    let any = user.is_admin || user.is_superuser;
    let deleted = sqlx::query(
        "UPDATE customer_todos SET deleted_at = NOW()
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL AND (? OR created_by = ?)",
    )
    .bind(id)
    .bind(user.company_id)
    .bind(any)
    .bind(user.id)
    .execute(&state.pool)
    .await?
    .rows_affected();
    if deleted == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// List a customer's to-dos
///
/// What the customer must do before install, from their template
/// appointments: open ones first, then done ones, each oldest first. For the
/// customer's page and their deals. Tick one off with `PUT
/// /v1/customer-todos/{id}`.
#[crate::openapi::path(
    get,
    path = "/v1/customers/{id}/customer-todos",
    operation_id = "list_customer_todos",
    tag = "customers",
    params(("id" = i32, Path, description = "Customer id")),
    responses(
        (status = 200, description = "The to-dos", body = Vec<CustomerTodo>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 404, description = "No such customer in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn customer_todos(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<Vec<CustomerTodo>>> {
    let pool = &state.pool;
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(pool)
    .await?;
    if found.is_none() {
        return Err(ApiError::NotFound);
    }
    let todos: Vec<CustomerTodo> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{TODO_SELECT} WHERE t.customer_id = ? AND t.company_id = ? AND t.deleted_at IS NULL
          ORDER BY t.done_at IS NOT NULL, t.created_at, t.id LIMIT 300"
    )))
    .bind(id)
    .bind(user.company_id)
    .fetch_all(pool)
    .await?;
    Ok(Json(todos))
}
