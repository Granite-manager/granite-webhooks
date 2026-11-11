use super::access::{STAGE_SQL, Stage, Viewer};
use super::schemas::{
    BoardQuery, Calendar, ConvertedDeal, DealProject, LinkableSale, Person, Project, ProjectAction,
    ProjectAppointment, ProjectFile, ProjectIssue, ProjectIssueCount, ProjectJobPhoto, ProjectNote,
    ProjectTask, ProjectWorkspace, ProjectsBoard, ReorderProjects, StageChange,
};
use super::service::{self, project_select, visible_project};
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::{JsonBody, MultiQuery};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use sqlx::{MySql, MySqlConnection, QueryBuilder};

const PAGE: u32 = 100;

/// `SELECT` of [`ProjectTask`] from `project_activities t`.
pub const TASK_SELECT: &str = "SELECT t.id, t.project_id, t.name, t.assigned_user_id,
        au.name AS assigned_user_name,
        DATE_FORMAT(t.deadline, '%Y-%m-%dT%H:%i:%s') AS deadline, CAST(t.priority AS CHAR) AS priority,
        DATE_FORMAT(t.completed_at, '%Y-%m-%dT%H:%i:%sZ') AS completed_at,
        t.created_by, cu.name AS created_by_name,
        DATE_FORMAT(t.created_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at
   FROM project_activities t
   LEFT JOIN users au ON au.id = t.assigned_user_id
   LEFT JOIN users cu ON cu.id = t.created_by";

/// The company's people to pick from; a check-in kiosk account (position 8)
/// is not a person and cannot be given work.
async fn people(conn: &mut MySqlConnection, company_id: i32) -> sqlx::Result<Vec<Person>> {
    sqlx::query_as(
        "SELECT id, COALESCE(NULLIF(TRIM(name), ''), email) AS name FROM users
          WHERE company_id = ? AND is_deleted = 0
            AND NOT EXISTS (SELECT 1 FROM users_positions kp
                             WHERE kp.user_id = users.id AND kp.position_id = 8)
          ORDER BY name, id",
    )
    .bind(company_id)
    .fetch_all(&mut *conn)
    .await
}

const APPOINTMENT_SELECT: &str = "SELECT e.id, e.project_id, cal.slug AS kind, e.title,
        DATE_FORMAT(e.start_date, '%Y-%m-%dT%H:%i:%sZ') AS start_date,
        COALESCE(e.unscheduled, 0) <> 0 AS unscheduled
   FROM events e
   JOIN calendars cal ON cal.id = e.calendar_id AND cal.deleted_at IS NULL
                     AND cal.slug IN ('templates', 'installations')";

/// The board
///
/// The caller's projects, as columns of the stages they work on: everything
/// for admins, Managers and Sales Managers; Awaiting Template to In
/// Fabrication for Office Managers; Templated onward for Shop Managers; their
/// own for sales reps.
#[crate::openapi::path(
    get,
    path = "/v1/projects",
    operation_id = "list_projects",
    tag = "projects",
    params(BoardQuery),
    responses(
        (status = 200, description = "The board", body = ProjectsBoard),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "The caller's position has no Projects board", body = ErrorBody),
        (status = 404, description = "Projects is off for the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn board(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<BoardQuery>,
) -> ApiResult<Json<ProjectsBoard>> {
    let mut viewer = Viewer::load(&state.pool, &user).await?;
    if query.employee {
        viewer = viewer.on_employee_side();
    }
    if !viewer.has_board() {
        return Err(ApiError::Denied(
            "Projects is for managers, sales, the office and the shop. Ask an admin to add your position.".into(),
        ));
    }
    let page = query.page.unwrap_or(1).clamp(1, 100_000);
    let mut sql = QueryBuilder::<MySql>::new(project_select());
    sql.push(" WHERE p.company_id = ")
        .push_bind(viewer.company_id);
    sql.push(format!(" AND {}", viewer.visible_sql()));
    if !query.archived {
        sql.push(" AND p.archived_at IS NULL");
    }
    sql.push(match query.view.as_deref() {
        Some("cancelled") => {
            " AND (p.cancelled_at IS NOT NULL OR s.cancelled_date IS NOT NULL)".to_string()
        }
        Some("completed") => format!(
            " AND p.cancelled_at IS NULL AND s.cancelled_date IS NULL AND {STAGE_SQL} = 'completed'"
        ),
        _ => format!(
            " AND p.cancelled_at IS NULL AND s.cancelled_date IS NULL AND {STAGE_SQL} <> 'completed'"
        ),
    });
    match query.owner.as_deref().map(str::trim) {
        Some("unassigned") => {
            sql.push(" AND p.owner_id IS NULL");
        }
        Some(owner) => {
            if let Ok(owner) = owner.parse::<i32>() {
                sql.push(" AND p.owner_id = ").push_bind(owner);
            }
        }
        None => {}
    }
    // The escaped search, with one.
    let mut searching: Option<String> = None;
    if let Some(q) = query.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        let term: String = q.chars().take(255).collect();
        let escaped = crate::sql::escape_like(&term);
        let like = format!("%{escaped}%");
        searching = Some(escaped);
        sql.push(" AND (p.title LIKE ")
            .push_bind(like.clone())
            .push(" OR c.name LIKE ")
            .push_bind(like.clone())
            .push(" OR q.quote_name LIKE ")
            .push_bind(like.clone())
            .push(" OR p.project_address LIKE ")
            .push_bind(like)
            .push(")");
    }
    if query.hold {
        sql.push(" AND p.is_on_hold <> 0");
    }
    sql.push(" ORDER BY ");
    // A search puts the caller's projects first, then a title or customer
    // name starting with it, then one containing it (before quote or address hits)
    if let Some(escaped) = searching {
        let prefix = format!("{escaped}%");
        let like = format!("%{escaped}%");
        sql.push("p.owner_id <=> ")
            .push_bind(viewer.user_id)
            .push(" DESC, CASE WHEN p.title LIKE ")
            .push_bind(prefix.clone())
            .push(" OR c.name LIKE ")
            .push_bind(prefix)
            .push(" THEN 0 WHEN p.title LIKE ")
            .push_bind(like.clone())
            .push(" OR c.name LIKE ")
            .push_bind(like)
            .push(" THEN 1 ELSE 2 END, ");
    }
    sql.push(format!("p.position, p.id LIMIT {} OFFSET ", PAGE + 1))
        .push_bind(i64::from(page - 1) * i64::from(PAGE));
    let mut projects: Vec<Project> = sql.build_query_as().fetch_all(&state.pool).await?;
    let has_next = projects.len() > PAGE as usize;
    projects.truncate(PAGE as usize);

    let mut conn = state.pool.acquire().await?;
    let owners = people(&mut conn, viewer.company_id).await?;
    let ids: Vec<i32> = projects.iter().map(|project| project.id).collect();
    let (tasks, appointments) = if ids.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        let mut tasks = QueryBuilder::<MySql>::new(TASK_SELECT);
        tasks
            .push(" WHERE t.company_id = ")
            .push_bind(viewer.company_id)
            .push(" AND t.deleted_at IS NULL AND t.completed_at IS NULL");
        crate::sql::push_in(&mut tasks, "t.project_id", &ids);
        // The caller's own task first: it is the one their card shows.
        tasks
            .push(" ORDER BY t.assigned_user_id <=> ")
            .push_bind(viewer.user_id)
            .push(" DESC, t.deadline IS NULL, t.deadline, t.id");
        let tasks: Vec<ProjectTask> = tasks.build_query_as().fetch_all(&mut *conn).await?;
        let mut next = QueryBuilder::<MySql>::new(APPOINTMENT_SELECT);
        next.push(" AND cal.company_id = ")
            .push_bind(viewer.company_id)
            .push(" WHERE e.deleted_date IS NULL AND e.unscheduled = 0 AND e.start_date >= UTC_TIMESTAMP()");
        crate::sql::push_in(&mut next, "e.project_id", &ids);
        next.push(" ORDER BY e.start_date, e.id");
        let appointments: Vec<ProjectAppointment> =
            next.build_query_as().fetch_all(&mut *conn).await?;
        (tasks, appointments)
    };
    super::auto::run(&state, viewer.company_id, &projects).await;
    let process_steps = super::process::company_steps(&mut conn, viewer.company_id).await?;
    let done_steps = super::process::done_steps(&mut conn, &ids).await?;
    let issue_counts: Vec<ProjectIssueCount> = if ids.is_empty() {
        Vec::new()
    } else {
        let mut sql = QueryBuilder::<MySql>::new(format!(
            "SELECT p.id AS project_id, COUNT(DISTINCT ei.id) AS count
               FROM projects p
               JOIN events e ON {PROJECT_EVENTS}
               JOIN event_issues ei ON ei.event_id = e.id AND ei.company_id = p.company_id
                                   AND ei.deleted_at IS NULL
              WHERE p.company_id = "
        ));
        sql.push_bind(viewer.company_id);
        crate::sql::push_in(&mut sql, "p.id", &ids);
        sql.push(" GROUP BY p.id");
        sql.build_query_as().fetch_all(&mut *conn).await?
    };
    Ok(Json(ProjectsBoard {
        projects,
        stages: Stage::ALL.to_vec(),
        owners,
        appointments,
        tasks,
        process_steps,
        done_steps,
        issue_counts,
        permissions: viewer.permissions,
        page,
        has_next,
    }))
}

/// Delete a project
///
/// Deletes the project for good, with its tasks, notes, files and history.
/// Its calendar events and the deal stay. Admins, Managers and Sales Managers.
#[crate::openapi::path(
    delete,
    path = "/v1/projects/{id}",
    operation_id = "delete_project",
    tag = "projects",
    params(("id" = i32, Path, description = "Project id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Only Managers and Sales Managers delete projects", body = ErrorBody),
        (status = 404, description = "No such project in the company, or Projects is off", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<axum::http::StatusCode> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    Viewer::require(viewer.permissions.delete)?;
    let company = viewer.company_id;
    let mut tx = state.pool.begin().await?;
    let found: Option<i32> =
        sqlx::query_scalar("SELECT id FROM projects WHERE id = ? AND company_id = ? FOR UPDATE")
            .bind(id)
            .bind(company)
            .fetch_optional(&mut *tx)
            .await?;
    found.ok_or(ApiError::NotFound)?;
    sqlx::query("UPDATE events SET project_id = NULL WHERE project_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    for table in [
        "project_activities",
        "project_notes",
        "project_files",
        "project_stage_history",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE project_id = ? AND company_id = ?"
        )))
        .bind(id)
        .bind(company)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("DELETE FROM projects WHERE id = ? AND company_id = ?")
        .bind(id)
        .bind(company)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// A project's page
///
/// The project, its tasks, notes, files, stage history, appointments and the
/// customer's transactions it may link to. Anyone with an open task on it may
/// open it too.
#[crate::openapi::path(
    get,
    path = "/v1/projects/{id}",
    operation_id = "get_project",
    tag = "projects",
    params(("id" = i32, Path, description = "Project id")),
    responses(
        (status = 200, description = "The project", body = ProjectWorkspace),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such project the caller may see, or Projects is off", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<ProjectWorkspace>> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    let project = visible_project(&mut conn, &viewer, id).await?;
    let company = viewer.company_id;
    let task_sql = format!(
        "{TASK_SELECT} WHERE t.project_id = ? AND t.company_id = ? AND t.deleted_at IS NULL
          ORDER BY t.completed_at IS NOT NULL, t.deadline IS NULL, t.deadline, t.id"
    );
    let tasks: Vec<ProjectTask> = sqlx::query_as(sqlx::AssertSqlSafe(task_sql))
        .bind(id)
        .bind(company)
        .fetch_all(&mut *conn)
        .await?;
    let notes: Vec<ProjectNote> = sqlx::query_as(
        "SELECT n.id, n.content, n.created_by, u.name AS author,
                DATE_FORMAT(n.created_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at
           FROM project_notes n LEFT JOIN users u ON u.id = n.created_by
          WHERE n.project_id = ? AND n.company_id = ? AND n.deleted_at IS NULL
          ORDER BY n.created_at DESC, n.id DESC",
    )
    .bind(id)
    .bind(company)
    .fetch_all(&mut *conn)
    .await?;
    let mut files: Vec<ProjectFile> = sqlx::query_as(
        "SELECT id, name, url, content_type, created_by,
                DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at
           FROM project_files
          WHERE project_id = ? AND company_id = ? AND deleted_at IS NULL ORDER BY id DESC",
    )
    .bind(id)
    .bind(company)
    .fetch_all(&mut *conn)
    .await?;
    for file in &mut files {
        file.url = state
            .storage
            .readable_url(&file.url, Some(file.content_type.as_str()))
            .await;
    }
    // The crew's job photos, from its appointments (the same ones as the
    // Completed lines of the history below).
    let job_photos: Vec<ProjectJobPhoto> = sqlx::query_as(
        "SELECT ei.id, ei.event_id,
                CASE WHEN cal.slug IN ('templates', 'installations') THEN cal.slug END AS kind,
                ei.url, u.name AS author,
                DATE_FORMAT(ei.created_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at
           FROM projects p
           JOIN events e ON e.deleted_date IS NULL
                        AND (e.project_id = p.id
                             OR (e.project_id IS NULL AND e.customer_id = p.customer_id
                                 AND e.created_date >= p.created_at - INTERVAL 1 DAY))
           JOIN event_images ei ON ei.event_id = e.id AND ei.company_id = p.company_id
                               AND ei.for_customer = 1 AND ei.deleted_at IS NULL
           LEFT JOIN calendars cal ON cal.id = e.calendar_id
           LEFT JOIN users u ON u.id = ei.created_by
          WHERE p.id = ? AND p.company_id = ?
          ORDER BY ei.created_at DESC, ei.id DESC LIMIT 200",
    )
    .bind(id)
    .bind(company)
    .fetch_all(&mut *conn)
    .await?;
    // The crew's issues, from the same jobs as the photos.
    let mut issues: Vec<ProjectIssue> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT ei.id, ei.event_id,
                CASE WHEN cal.slug IN ('templates', 'installations') THEN cal.slug END AS kind,
                IF(e.unscheduled = 1, NULL, DATE_FORMAT(e.start_date, '%Y-%m-%dT%H:%i:%sZ')) AS job_date,
                ei.description, ei.created_by_name AS author,
                DATE_FORMAT(ei.created_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at
           FROM projects p
           JOIN events e ON {PROJECT_EVENTS}
           JOIN event_issues ei ON ei.event_id = e.id AND ei.company_id = p.company_id
                               AND ei.deleted_at IS NULL
           LEFT JOIN calendars cal ON cal.id = e.calendar_id
          WHERE p.id = ? AND p.company_id = ?
          ORDER BY ei.created_at DESC, ei.id DESC LIMIT 200"
    )))
    .bind(id)
    .bind(company)
    .fetch_all(&mut *conn)
    .await?;
    if !issues.is_empty() {
        let ids: Vec<i32> = issues.iter().map(|issue| issue.id).collect();
        let mut sql =
            QueryBuilder::<MySql>::new("SELECT issue_id, url FROM event_issue_images WHERE 1 = 1");
        crate::sql::push_in(&mut sql, "issue_id", &ids);
        sql.push(" ORDER BY id");
        let photos: Vec<(i32, String)> = sql.build_query_as().fetch_all(&mut *conn).await?;
        for (issue_id, url) in photos {
            if let Some(issue) = issues.iter_mut().find(|issue| issue.id == issue_id) {
                issue.photos.push(url);
            }
        }
    }
    // Stage moves, and the template maker's and installer's Completed (the
    // jobs that move it on their own, see `STAGE_SQL`).
    let history: Vec<StageChange> = sqlx::query_as(
        "SELECT id, from_stage, to_stage, source_action, reason, author, created_at FROM (
           SELECT h.id, h.from_stage, h.to_stage, h.source_action, h.reason, u.name AS author,
                  DATE_FORMAT(h.created_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at, h.created_at AS at
             FROM project_stage_history h LEFT JOIN users u ON u.id = h.actor_id
            WHERE h.project_id = ? AND h.company_id = ?
           UNION ALL
           SELECT -e.id, NULL, IF(cal.slug = 'templates', 'templated', 'installed'),
                  IF(cal.slug = 'templates', 'template-completed', 'install-completed'),
                  NULL, u.name, DATE_FORMAT(e.completed_at, '%Y-%m-%dT%H:%i:%sZ'), e.completed_at
             FROM projects p
             JOIN events e ON e.deleted_date IS NULL AND e.crew_status = 'completed'
                          AND e.completed_at IS NOT NULL
                          AND (e.project_id = p.id
                               OR (e.project_id IS NULL AND e.customer_id = p.customer_id
                                   AND e.created_date >= p.created_at - INTERVAL 1 DAY))
             JOIN calendars cal ON cal.id = e.calendar_id
                               AND cal.slug IN ('templates', 'installations')
             LEFT JOIN users u ON u.id = e.assigned_user_id
            WHERE p.id = ? AND p.company_id = ?
         ) entries ORDER BY at DESC LIMIT 100",
    )
    .bind(id)
    .bind(company)
    .bind(id)
    .bind(company)
    .fetch_all(&mut *conn)
    .await?;
    let sales: Vec<LinkableSale> = if project.sale_id.is_some() {
        Vec::new()
    } else {
        sqlx::query_as(
            "SELECT s.id, CAST(s.price AS DOUBLE) AS price, s.project_address,
                    DATE_FORMAT(s.installed_date, '%Y-%m-%dT%H:%i:%sZ') AS installed_date,
                    DATE_FORMAT(s.paid_date, '%Y-%m-%dT%H:%i:%sZ') AS paid_date
               FROM sales s
              WHERE s.company_id = ? AND s.customer_id = ? AND s.cancelled_date IS NULL
                AND NOT EXISTS (SELECT 1 FROM projects lp WHERE lp.sale_id = s.id)
              ORDER BY s.id DESC LIMIT 100",
        )
        .bind(company)
        .bind(project.customer_id)
        .fetch_all(&mut *conn)
        .await?
    };
    let calendars: Vec<Calendar> = sqlx::query_as(
        "SELECT id, slug, name FROM calendars
          WHERE company_id = ? AND deleted_at IS NULL AND slug IN ('templates', 'installations')
          ORDER BY slug = 'installations', id",
    )
    .bind(company)
    .fetch_all(&mut *conn)
    .await?;
    let appointment_sql = format!(
        "{APPOINTMENT_SELECT} AND cal.company_id = ?
          WHERE e.project_id = ? AND e.deleted_date IS NULL
          ORDER BY e.unscheduled DESC, e.start_date DESC, e.id DESC LIMIT 100"
    );
    let appointments: Vec<ProjectAppointment> =
        sqlx::query_as(sqlx::AssertSqlSafe(appointment_sql))
            .bind(company)
            .bind(id)
            .fetch_all(&mut *conn)
            .await?;
    super::auto::run(&state, company, std::slice::from_ref(&project)).await;
    let process_steps = super::process::company_steps(&mut conn, company).await?;
    let done_steps = super::process::done_steps(&mut conn, &[id]).await?;
    Ok(Json(ProjectWorkspace {
        project,
        tasks,
        task_times: super::tasks::task_times(&mut conn, company).await?,
        notes,
        files,
        job_photos,
        issues,
        history,
        sales,
        people: people(&mut conn, company).await?,
        calendars,
        appointments,
        process_steps,
        done_steps,
        permissions: viewer.permissions,
        user_id: viewer.user_id,
    }))
}

/// Act on a project
///
/// Moves it forward, links its transaction, confirms payment, picks the
/// owner, puts it on hold, cancels, archives, or corrects its stage or
/// payment with a reason. A stale `version` gets a 409 saying to refresh.
#[crate::openapi::path(
    post,
    path = "/v1/projects/{id}/actions",
    operation_id = "act_on_project",
    tag = "projects",
    params(("id" = i32, Path, description = "Project id")),
    request_body = ProjectAction,
    responses(
        (status = 200, description = "The project after the action", body = Project),
        (status = 400, description = "Bad date, missing reason, or a move with its own button", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "The caller's position can't do this", body = ErrorBody),
        (status = 404, description = "No such project the caller may see", body = ErrorBody),
        (status = 409, description = "Changed since the page loaded, or not possible now (the message says why)", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn act(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(action): JsonBody<ProjectAction>,
) -> ApiResult<Json<Project>> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    service::apply(&state.pool, &viewer, id, action)
        .await
        .map(Json)
}

/// Save a column's manual order
///
/// Numbers the projects sent from 0, in the order sent. Only the order
/// changes, so a page already open can still act on them. Every project must
/// belong to the company, otherwise nothing is saved.
#[crate::openapi::path(
    post,
    path = "/v1/projects/reorder",
    operation_id = "reorder_projects",
    tag = "projects",
    request_body = ReorderProjects,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "No projects, or more than a page of them", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "The caller's position can't move projects", body = ErrorBody),
        (status = 404, description = "A project is not one of the company's", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn reorder(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(body): JsonBody<ReorderProjects>,
) -> ApiResult<axum::http::StatusCode> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    Viewer::require(viewer.permissions.production)?;
    if body.ids.is_empty() || body.ids.len() > PAGE as usize {
        return Err(ApiError::BadRequest(
            "Send one column's projects to reorder".into(),
        ));
    }
    let company = viewer.company_id;
    let mut tx = state.pool.begin().await?;
    let mut owned = QueryBuilder::<MySql>::new("SELECT COUNT(*) FROM projects WHERE company_id = ");
    owned.push_bind(company).push(" AND id IN (");
    let mut list = owned.separated(", ");
    for id in &body.ids {
        list.push_bind(*id);
    }
    owned.push(")");
    let found: i64 = owned.build_query_scalar().fetch_one(&mut *tx).await?;
    if usize::try_from(found).ok() != Some(body.ids.len()) {
        return Err(ApiError::NotFound);
    }
    for (position, id) in body.ids.iter().enumerate() {
        sqlx::query("UPDATE projects SET position = ? WHERE id = ? AND company_id = ?")
            .bind(i32::try_from(position).unwrap_or(i32::MAX))
            .bind(id)
            .bind(company)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// A deal's project
///
/// The won deal's project, if any, and whether the caller may start one.
#[crate::openapi::path(
    get,
    path = "/v1/deals/{id}/project",
    operation_id = "get_deal_project",
    tag = "projects",
    params(("id" = i64, Path, description = "Deal id")),
    responses(
        (status = 200, description = "The deal's project", body = DealProject),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "Projects is off for the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn deal_project(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<DealProject>> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    let project_id = service::deal_project(&mut conn, viewer.company_id, id).await?;
    Ok(Json(DealProject {
        project_id,
        can_convert: viewer.permissions.convert,
    }))
}

/// Start a deal's project
///
/// Turns a won deal into a project (Awaiting Template). Won deals start one
/// on their own; this is for deals won before Projects was on. Answers the
/// existing project when the deal has one.
#[crate::openapi::path(
    post,
    path = "/v1/deals/{id}/project",
    operation_id = "create_deal_project",
    tag = "projects",
    params(("id" = i64, Path, description = "Deal id")),
    responses(
        (status = 200, description = "The deal's project", body = ConvertedDeal),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "The caller's position can't start projects", body = ErrorBody),
        (status = 404, description = "No such deal the caller may see, or Projects is off", body = ErrorBody),
        (status = 409, description = "The deal is lost", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn convert_deal(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<ConvertedDeal>> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let (project_id, created) = service::convert_deal(&state.pool, &viewer, id).await?;
    Ok(Json(ConvertedDeal {
        project_id,
        created,
    }))
}

/// Joins a project (`p`) to its jobs (`e`): those booked on it, and the
/// customer's from a day before it started (as the job photos and history).
const PROJECT_EVENTS: &str = "e.deleted_date IS NULL
    AND (e.project_id = p.id
         OR (e.project_id IS NULL AND e.customer_id = p.customer_id
             AND e.created_date >= p.created_at - INTERVAL 1 DAY))";
