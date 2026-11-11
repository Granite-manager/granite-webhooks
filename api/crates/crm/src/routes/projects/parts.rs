//! A project's parts: Countertops, a full-height backsplash, Cabinets. Each
//! part goes through the stages on its own, with its own template,
//! fabrication and installation, and the project sits in the stage of its
//! least finished part. A project gets parts the first time someone adds one:
//! the work it already has becomes its first part. A part's type says which
//! stages it uses (`/admin/projects`): Cabinets have no template and no
//! fabrication, so they only wait for their installation.

use super::access::part_job_date_sql;
use super::access::{
    PART_EVENTS_SQL, PART_INDEX_SQL, PROJECT_JOB_SQL, Stage, Viewer, check_transition,
};
use super::schemas::{
    AppointmentPart, PartInput, PartType, PartTypeInput, PartUpdate, Project, ProjectPart,
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

const NAME_MAX: usize = 80;

/// The types a company starts with: name, templated, fabricated.
const DEFAULT_TYPES: [(&str, bool, bool); 3] = [
    ("Countertops", true, true),
    ("Backsplash", true, true),
    ("Cabinets", false, false),
];

fn conflict(message: impl Into<String>) -> ApiError {
    ApiError::Conflict(message.into())
}

/// The name to store, or why it can't be.
fn clean_name(name: &str) -> ApiResult<&str> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest("Give it a name".into()));
    }
    if name.chars().count() > NAME_MAX {
        return Err(ApiError::BadRequest(format!(
            "A name can be {NAME_MAX} characters at most"
        )));
    }
    Ok(name)
}

/// The company's part types, in order. A company that never had any gets
/// Countertops, Backsplash and Cabinets.
pub async fn part_types(
    conn: &mut MySqlConnection,
    company_id: i32,
) -> sqlx::Result<Vec<PartType>> {
    let any: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM project_part_types WHERE company_id = ?")
            .bind(company_id)
            .fetch_one(&mut *conn)
            .await?;
    if any == 0 {
        for (position, (name, template, fabrication)) in DEFAULT_TYPES.iter().enumerate() {
            sqlx::query(
                "INSERT INTO project_part_types
                    (company_id, name, uses_template, uses_fabrication, position)
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(company_id)
            .bind(name)
            .bind(template)
            .bind(fabrication)
            .bind(i32::try_from(position).unwrap_or_default())
            .execute(&mut *conn)
            .await?;
        }
    }
    sqlx::query_as(
        "SELECT id, name, uses_template <> 0 AS uses_template,
                uses_fabrication <> 0 AS uses_fabrication, position
           FROM project_part_types
          WHERE company_id = ? AND deleted_at IS NULL
          ORDER BY position, id",
    )
    .bind(company_id)
    .fetch_all(&mut *conn)
    .await
}

/// The parts of these projects, each with the stage it is in.
pub async fn parts_of(
    conn: &mut MySqlConnection,
    company_id: i32,
    project_ids: &[i32],
) -> sqlx::Result<Vec<ProjectPart>> {
    if project_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut sql = QueryBuilder::<MySql>::new(format!(
        "SELECT pp.id, pp.project_id, pp.type_id, pt.name AS type_name, pp.name,
                pt.uses_template <> 0 AS uses_template,
                pt.uses_fabrication <> 0 AS uses_fabrication,
                ELT({PART_INDEX_SQL}, 'new', 'templated', 'fabrication', 'installed') AS stage_code,
                DATE_FORMAT(pp.fabricated_at, '%Y-%m-%dT%H:%i:%sZ') AS fabricated_at,
                {template} AS template_date, {install} AS install_date, pp.position
           FROM project_parts pp
           JOIN project_part_types pt ON pt.id = pp.type_id
           JOIN projects p ON p.id = pp.project_id
           LEFT JOIN sales s ON s.id = p.sale_id AND s.company_id = p.company_id
          WHERE pp.company_id = ",
        template = part_job_date_sql("templates"),
        install = part_job_date_sql("installations"),
    ));
    sql.push_bind(company_id);
    crate::sql::push_in(&mut sql, "pp.project_id", project_ids);
    sql.push(" ORDER BY pp.project_id, pp.position, pp.id");
    sql.build_query_as().fetch_all(&mut *conn).await
}

/// Gives the templates and installations booked for no part to a part, on
/// the projects that have parts: the first part (of a type that is templated,
/// for a template) with no such job yet, else the first one. So a template
/// booked after a backsplash was added is the backsplash's. Anyone can move a
/// job to another part on the project's page.
pub async fn adopt_jobs(
    conn: &mut MySqlConnection,
    company_id: i32,
    project_ids: &[i32],
) -> sqlx::Result<()> {
    if project_ids.is_empty() {
        return Ok(());
    }
    let mut sql = QueryBuilder::<MySql>::new(format!(
        "SELECT e.id, p.id, cal.slug
           FROM projects p
           JOIN events e ON {PROJECT_JOB_SQL} AND e.project_part_id IS NULL
           JOIN calendars cal ON cal.id = e.calendar_id
                             AND cal.slug IN ('templates', 'installations')
          WHERE EXISTS (SELECT 1 FROM project_parts pp WHERE pp.project_id = p.id)
            AND p.company_id = "
    ));
    sql.push_bind(company_id);
    crate::sql::push_in(&mut sql, "p.id", project_ids);
    sql.push(" ORDER BY e.created_date, e.id");
    let jobs: Vec<(i32, i32, String)> = sql.build_query_as().fetch_all(&mut *conn).await?;
    for (event_id, project_id, slug) in jobs {
        let part: Option<i32> = sqlx::query_scalar(
            "SELECT pp.id FROM project_parts pp
               JOIN project_part_types pt ON pt.id = pp.type_id
              WHERE pp.project_id = ? AND (? = 'installations' OR pt.uses_template <> 0)
              ORDER BY EXISTS (SELECT 1 FROM events x
                                 JOIN calendars xc ON xc.id = x.calendar_id AND xc.slug = ?
                                WHERE x.project_part_id = pp.id AND x.deleted_date IS NULL),
                       pp.position, pp.id
              LIMIT 1",
        )
        .bind(project_id)
        .bind(&slug)
        .bind(&slug)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(part) = part {
            sqlx::query(
                "UPDATE events SET project_part_id = ?, project_id = ?
                  WHERE id = ? AND project_part_id IS NULL",
            )
            .bind(part)
            .bind(project_id)
            .bind(event_id)
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

/// The part an action is for: the one named, else the project's least
/// finished part (the one that holds the project in its stage).
pub fn target(parts: &[ProjectPart], part_id: Option<i32>) -> ApiResult<&ProjectPart> {
    match part_id {
        Some(id) => parts
            .iter()
            .find(|part| part.id == id)
            .ok_or_else(|| ApiError::BadRequest("That part isn't on this project".into())),
        None => parts
            .iter()
            .min_by_key(|part| part.stage_code)
            .ok_or(ApiError::NotFound),
    }
}

/// The first stage a part of this type can be in.
const fn first_stage(part: &ProjectPart) -> Stage {
    match (part.uses_template, part.uses_fabrication) {
        (true, _) => Stage::New,
        (false, true) => Stage::Templated,
        (false, false) => Stage::Fabrication,
    }
}

async fn history(
    conn: &mut MySqlConnection,
    viewer: &Viewer,
    part: &ProjectPart,
    change: (Option<Stage>, Stage),
    source: &str,
    reason: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO project_stage_history
            (project_id, company_id, from_stage, to_stage, actor_id, source_action, reason, part_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(part.project_id)
    .bind(viewer.company_id)
    .bind(change.0.map(Stage::as_str))
    .bind(change.1.as_str())
    .bind(viewer.user_id)
    .bind(source)
    .bind(reason)
    .bind(part.id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn bump(conn: &mut MySqlConnection, company_id: i32, project_id: i32) -> sqlx::Result<()> {
    sqlx::query("UPDATE projects SET version = version + 1 WHERE id = ? AND company_id = ?")
        .bind(project_id)
        .bind(company_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// A stage the part's type has no use for.
fn check_stage(part: &ProjectPart, to: Stage) -> ApiResult<()> {
    if to == Stage::Completed {
        return Err(conflict(
            "Paid & Completed is for the whole project, once every part is installed and it is paid",
        ));
    }
    if to < first_stage(part) {
        return Err(conflict(format!(
            "{} has no {} stage",
            part.name,
            to.label()
        )));
    }
    Ok(())
}

/// Moves one part to a stage by hand (the project's Stage action).
pub async fn move_part(
    conn: &mut MySqlConnection,
    viewer: &Viewer,
    project: &Project,
    part: &ProjectPart,
    to: Stage,
) -> ApiResult<()> {
    check_stage(part, to)?;
    let from = part.stage_code;
    check_transition(from, to)?;
    if to == from {
        return Ok(());
    }
    let back = to < from;
    if !back && to == Stage::Fabrication && part.uses_fabrication && project.sale_id.is_none() {
        return Err(conflict("Link the project's transaction first"));
    }
    // Moved back by hand: it stays there until someone moves it forward.
    sqlx::query("UPDATE project_parts SET stage_code = ?, stage_cap = ? WHERE id = ?")
        .bind(to.as_str())
        .bind(back.then(|| to.as_str()))
        .bind(part.id)
        .execute(&mut *conn)
        .await?;
    bump(conn, viewer.company_id, project.id).await?;
    let source = if back { "moved-back" } else { "stage-change" };
    history(conn, viewer, part, (Some(from), to), source, None).await?;
    Ok(())
}

/// The shop finished fabricating one part.
pub async fn fabricated(
    conn: &mut MySqlConnection,
    viewer: &Viewer,
    part: &ProjectPart,
) -> ApiResult<()> {
    if !part.uses_fabrication {
        return Err(conflict(format!("{} isn't fabricated", part.name)));
    }
    if part.stage_code < Stage::Templated {
        return Err(conflict("Template the job before it is fabricated"));
    }
    if part.fabricated_at.is_some() {
        return Ok(());
    }
    sqlx::query("UPDATE project_parts SET fabricated_at = UTC_TIMESTAMP() WHERE id = ?")
        .bind(part.id)
        .execute(&mut *conn)
        .await?;
    bump(conn, viewer.company_id, part.project_id).await?;
    let to = part.stage_code.max(Stage::Fabrication);
    history(
        conn,
        viewer,
        part,
        (Some(part.stage_code), to),
        "fabricated",
        None,
    )
    .await?;
    Ok(())
}

/// A manager moves one part back, with a reason.
pub async fn correct(
    conn: &mut MySqlConnection,
    viewer: &Viewer,
    project: &Project,
    part: &ProjectPart,
    to: Stage,
    reason: &str,
) -> ApiResult<()> {
    if to >= Stage::Installed {
        return Err(ApiError::BadRequest(
            "Installation and payment are confirmed with their own buttons".into(),
        ));
    }
    check_stage(part, to)?;
    if to == Stage::Fabrication && part.uses_fabrication && project.sale_id.is_none() {
        return Err(conflict("Link the project's transaction first"));
    }
    if let (Some(sale), Some(_)) = (project.sale_id, &project.installed_at) {
        // The transaction's installed date goes; the other parts it had
        // installed keep that as their own stage.
        sqlx::query(
            "UPDATE project_parts pp JOIN sales s ON s.id = ?
                SET pp.stage_code = 'installed'
              WHERE pp.project_id = ? AND pp.id <> ? AND pp.created_at <= s.installed_date",
        )
        .bind(sale)
        .bind(project.id)
        .bind(part.id)
        .execute(&mut *conn)
        .await?;
        sqlx::query("UPDATE sales SET installed_date = NULL WHERE id = ? AND company_id = ?")
            .bind(sale)
            .bind(viewer.company_id)
            .execute(&mut *conn)
            .await?;
    }
    sqlx::query("UPDATE project_parts SET stage_code = ?, stage_cap = ? WHERE id = ?")
        .bind(to.as_str())
        .bind(to.as_str())
        .bind(part.id)
        .execute(&mut *conn)
        .await?;
    bump(conn, viewer.company_id, project.id).await?;
    history(
        conn,
        viewer,
        part,
        (Some(part.stage_code), to),
        "manager-correction",
        Some(reason),
    )
    .await?;
    Ok(())
}

async fn require_type(
    conn: &mut MySqlConnection,
    company_id: i32,
    type_id: i32,
) -> ApiResult<String> {
    sqlx::query_scalar(
        "SELECT name FROM project_part_types
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(type_id)
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| ApiError::BadRequest("Pick a part type".into()))
}

/// The company's part types
///
/// What a project's parts can be, and the stages each one uses. A company
/// starts with Countertops, Backsplash and Cabinets.
#[crate::openapi::path(
    get,
    path = "/v1/projects/part-types",
    operation_id = "list_project_part_types",
    tag = "projects",
    responses(
        (status = 200, description = "The part types, in order", body = Vec<PartType>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_types(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Vec<PartType>>> {
    let mut conn = state.pool.acquire().await?;
    Ok(Json(part_types(&mut conn, user.company_id).await?))
}

/// Add a part type
#[crate::openapi::path(
    post,
    path = "/v1/projects/part-types",
    operation_id = "create_project_part_type",
    tag = "projects",
    request_body = PartTypeInput,
    responses(
        (status = 201, description = "Added", body = CreatedId),
        (status = 400, description = "Missing or long name", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_type(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<PartTypeInput>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    let name = clean_name(&input.name)?;
    let mut conn = state.pool.acquire().await?;
    // The company's first types exist before its own are added.
    part_types(&mut conn, user.company_id).await?;
    let id = sqlx::query(
        "INSERT INTO project_part_types
            (company_id, name, uses_template, uses_fabrication, position)
         SELECT ?, ?, ?, ?, COALESCE(MAX(position) + 1, 0)
           FROM project_part_types WHERE company_id = ?",
    )
    .bind(user.company_id)
    .bind(name)
    .bind(input.uses_template)
    .bind(input.uses_fabrication)
    .bind(user.company_id)
    .execute(&mut *conn)
    .await?
    .last_insert_id();
    Ok((StatusCode::CREATED, Json(CreatedId { id: id as i64 })))
}

/// Change a part type
///
/// Its name and the stages it uses; every part of that type follows.
#[crate::openapi::path(
    patch,
    path = "/v1/projects/part-types/{type_id}",
    operation_id = "update_project_part_type",
    tag = "projects",
    params(("type_id" = i32, Path, description = "Part type id")),
    request_body = PartTypeInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Missing or long name", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such part type in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_type(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(type_id): Path<i32>,
    JsonBody(input): JsonBody<PartTypeInput>,
) -> ApiResult<StatusCode> {
    let name = clean_name(&input.name)?;
    let changed = sqlx::query(
        "UPDATE project_part_types SET name = ?, uses_template = ?, uses_fabrication = ?
          WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(name)
    .bind(input.uses_template)
    .bind(input.uses_fabrication)
    .bind(type_id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?
    .rows_affected();
    if changed == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a part type
///
/// It can no longer be picked; the parts that already have it keep it.
#[crate::openapi::path(
    delete,
    path = "/v1/projects/part-types/{type_id}",
    operation_id = "delete_project_part_type",
    tag = "projects",
    params(("type_id" = i32, Path, description = "Part type id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such part type in the company", body = ErrorBody),
        (status = 409, description = "It is the company's last part type", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_type(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(type_id): Path<i32>,
) -> ApiResult<StatusCode> {
    let mut conn = state.pool.acquire().await?;
    require_type(&mut conn, user.company_id, type_id)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let left: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM project_part_types
          WHERE company_id = ? AND deleted_at IS NULL AND id <> ?",
    )
    .bind(user.company_id)
    .bind(type_id)
    .fetch_one(&mut *conn)
    .await?;
    if left == 0 {
        return Err(conflict("Keep at least one part type"));
    }
    sqlx::query(
        "UPDATE project_part_types SET deleted_at = UTC_TIMESTAMP()
          WHERE id = ? AND company_id = ?",
    )
    .bind(type_id)
    .bind(user.company_id)
    .execute(&mut *conn)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// A project the caller may change the parts of, open and not cancelled.
async fn editable(conn: &mut MySqlConnection, viewer: &Viewer, id: i32) -> ApiResult<Project> {
    let perms = viewer.permissions;
    Viewer::require(perms.production || perms.manage || perms.convert)?;
    let project = visible_project(conn, viewer, id).await?;
    if project.archived_at.is_some()
        || project.cancelled_at.is_some()
        || project.sale_cancelled_at.is_some()
    {
        return Err(conflict(
            "This project is archived or cancelled, so its parts can't change.",
        ));
    }
    Ok(project)
}

/// Add a part to a project
///
/// The part starts at the first stage its type uses, so the project goes back
/// there. The first time, the work the project already has becomes its first
/// part (`first_type_id`, else the company's first type), in the stage the
/// project is in, with the steps it has done and its jobs.
#[crate::openapi::path(
    post,
    path = "/v1/projects/{id}/parts",
    operation_id = "add_project_part",
    tag = "projects",
    params(("id" = i32, Path, description = "Project id")),
    request_body = PartInput,
    responses(
        (status = 201, description = "Added", body = CreatedId),
        (status = 400, description = "Unknown part type, or a long name", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "The caller's position can't change a project's parts", body = ErrorBody),
        (status = 404, description = "No such project the caller may see", body = ErrorBody),
        (status = 409, description = "The project is archived or cancelled", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn add_part(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<PartInput>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let company = viewer.company_id;
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT id FROM projects WHERE id = ? AND company_id = ? FOR UPDATE")
        .bind(id)
        .bind(company)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(ApiError::NotFound)?;
    let project = editable(&mut tx, &viewer, id).await?;
    let types = part_types(&mut tx, company).await?;
    let type_name = require_type(&mut tx, company, input.type_id).await?;
    let name = match input.name.as_deref().map(str::trim) {
        Some(name) if !name.is_empty() => clean_name(name)?.to_string(),
        _ => type_name,
    };
    let existing: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM project_parts WHERE project_id = ?")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    if existing == 0 {
        // The work the project already has becomes its first part.
        let first_type = match input.first_type_id {
            Some(type_id) => {
                require_type(&mut tx, company, type_id).await?;
                type_id
            }
            None => types
                .first()
                .map(|first| first.id)
                .ok_or_else(|| ApiError::BadRequest("Pick a part type".into()))?,
        };
        let stage = project.stage_code.min(Stage::Installed);
        let first = sqlx::query(
            "INSERT INTO project_parts
                (project_id, company_id, type_id, name, stage_code, stage_cap, fabricated_at,
                 position, created_by, created_at)
             SELECT p.id, p.company_id, pt.id, pt.name, ?,
                    CASE p.stage_cap WHEN 'template_scheduled' THEN 'new'
                                     WHEN 'install_scheduled' THEN 'fabrication'
                                     WHEN 'completed' THEN NULL
                                     ELSE p.stage_cap END,
                    p.fabricated_at, 0, p.created_by, p.created_at
               FROM projects p JOIN project_part_types pt ON pt.id = ?
              WHERE p.id = ?",
        )
        .bind(stage.as_str())
        .bind(first_type)
        .bind(id)
        .execute(&mut *tx)
        .await?
        .last_insert_id();
        for table in [
            "project_process_steps_done",
            "project_process_step_auto_off",
        ] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE {table} SET part_id = ? WHERE project_id = ? AND part_id = 0"
            )))
            .bind(first)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        // Its jobs so far are that part's; later ones find theirs (`adopt_jobs`).
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE events e JOIN projects p ON p.id = ?
                SET e.project_part_id = ?, e.project_id = p.id
              WHERE {PROJECT_JOB_SQL} AND e.project_part_id IS NULL"
        )))
        .bind(id)
        .bind(first)
        .execute(&mut *tx)
        .await?;
    }
    let part_id = sqlx::query(
        "INSERT INTO project_parts
            (project_id, company_id, type_id, name, position, created_by, created_at)
         SELECT ?, ?, ?, ?, COALESCE(MAX(position) + 1, 0), ?, UTC_TIMESTAMP()
           FROM project_parts WHERE project_id = ?",
    )
    .bind(id)
    .bind(company)
    .bind(input.type_id)
    .bind(&name)
    .bind(viewer.user_id)
    .bind(id)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    let part_id = i32::try_from(part_id).map_err(|_| ApiError::Internal("part id".into()))?;
    bump(&mut tx, company, id).await?;
    let parts = parts_of(&mut tx, company, &[id]).await?;
    if let Some(part) = parts.iter().find(|part| part.id == part_id) {
        history(
            &mut tx,
            &viewer,
            part,
            (None, part.stage_code),
            "part-added",
            None,
        )
        .await?;
    }
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedId {
            id: i64::from(part_id),
        }),
    ))
}

/// The project's part `part_id`, or 404.
async fn require_part(
    conn: &mut MySqlConnection,
    company_id: i32,
    project_id: i32,
    part_id: i32,
) -> ApiResult<()> {
    sqlx::query_scalar::<_, i32>(
        "SELECT id FROM project_parts WHERE id = ? AND project_id = ? AND company_id = ?",
    )
    .bind(part_id)
    .bind(project_id)
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(ApiError::NotFound)?;
    Ok(())
}

/// Rename a part, or change its type
#[crate::openapi::path(
    patch,
    path = "/v1/projects/{id}/parts/{part_id}",
    operation_id = "update_project_part",
    tag = "projects",
    params(
        ("id" = i32, Path, description = "Project id"),
        ("part_id" = i32, Path, description = "Part id"),
    ),
    request_body = PartUpdate,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Unknown part type, or a missing or long name", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "The caller's position can't change a project's parts", body = ErrorBody),
        (status = 404, description = "No such project the caller may see, or part on it", body = ErrorBody),
        (status = 409, description = "The project is archived or cancelled", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_part(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, part_id)): Path<(i32, i32)>,
    JsonBody(input): JsonBody<PartUpdate>,
) -> ApiResult<StatusCode> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let company = viewer.company_id;
    let mut conn = state.pool.acquire().await?;
    editable(&mut conn, &viewer, id).await?;
    require_part(&mut conn, company, id, part_id).await?;
    if let Some(name) = input.name.as_deref() {
        sqlx::query("UPDATE project_parts SET name = ? WHERE id = ?")
            .bind(clean_name(name)?)
            .bind(part_id)
            .execute(&mut *conn)
            .await?;
    }
    if let Some(type_id) = input.type_id {
        require_type(&mut conn, company, type_id).await?;
        sqlx::query("UPDATE project_parts SET type_id = ? WHERE id = ?")
            .bind(type_id)
            .bind(part_id)
            .execute(&mut *conn)
            .await?;
    }
    bump(&mut conn, company, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Remove a part from a project
///
/// With the steps checked off for it. A part with jobs on the schedule stays
/// until they are moved to another part or deleted, and a project keeps its
/// last part.
#[crate::openapi::path(
    delete,
    path = "/v1/projects/{id}/parts/{part_id}",
    operation_id = "delete_project_part",
    tag = "projects",
    params(
        ("id" = i32, Path, description = "Project id"),
        ("part_id" = i32, Path, description = "Part id"),
    ),
    responses(
        (status = 204, description = "Removed"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "The caller's position can't change a project's parts", body = ErrorBody),
        (status = 404, description = "No such project the caller may see, or part on it", body = ErrorBody),
        (status = 409, description = "It has jobs, is the last part, or the project is archived or cancelled", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_part(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, part_id)): Path<(i32, i32)>,
) -> ApiResult<StatusCode> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let company = viewer.company_id;
    let mut tx = state.pool.begin().await?;
    editable(&mut tx, &viewer, id).await?;
    require_part(&mut tx, company, id, part_id).await?;
    let parts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM project_parts WHERE project_id = ?")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    if parts <= 1 {
        return Err(conflict("A project keeps its last part"));
    }
    let jobs: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM project_parts pp
           JOIN projects p ON p.id = pp.project_id
           JOIN events e ON e.deleted_date IS NULL AND {PART_EVENTS_SQL}
           JOIN calendars cal ON cal.id = e.calendar_id
                             AND cal.slug IN ('templates', 'installations')
          WHERE pp.id = ?"
    )))
    .bind(part_id)
    .fetch_one(&mut *tx)
    .await?;
    if jobs > 0 {
        return Err(conflict(
            "This part has jobs on the schedule. Move them to another part or delete them first.",
        ));
    }
    for table in [
        "project_process_steps_done",
        "project_process_step_auto_off",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE project_id = ? AND part_id = ?"
        )))
        .bind(id)
        .bind(part_id)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("DELETE FROM project_parts WHERE id = ? AND project_id = ?")
        .bind(part_id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    bump(&mut tx, company, id).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Say which part a template or installation is for
#[crate::openapi::path(
    put,
    path = "/v1/projects/{id}/appointments/{event_id}/part",
    operation_id = "set_project_appointment_part",
    tag = "projects",
    params(
        ("id" = i32, Path, description = "Project id"),
        ("event_id" = i32, Path, description = "The template or installation (event id)"),
    ),
    request_body = AppointmentPart,
    responses(
        (status = 204, description = "Saved"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "The caller's position can't change a project's parts", body = ErrorBody),
        (status = 404, description = "No such project the caller may see, part or job on it", body = ErrorBody),
        (status = 409, description = "The project is archived or cancelled", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_appointment_part(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, event_id)): Path<(i32, i32)>,
    JsonBody(input): JsonBody<AppointmentPart>,
) -> ApiResult<StatusCode> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let company = viewer.company_id;
    let mut conn = state.pool.acquire().await?;
    editable(&mut conn, &viewer, id).await?;
    require_part(&mut conn, company, id, input.part_id).await?;
    let changed = sqlx::query(
        "UPDATE events SET project_part_id = ?
          WHERE id = ? AND project_id = ? AND deleted_date IS NULL",
    )
    .bind(input.part_id)
    .bind(event_id)
    .bind(id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if changed == 0 {
        return Err(ApiError::NotFound);
    }
    bump(&mut conn, company, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
