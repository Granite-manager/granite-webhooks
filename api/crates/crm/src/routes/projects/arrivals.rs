//! A step's people (Admin > Projects > Notify) are told when the step comes
//! up: it becomes the first of the company's steps for the project's stage
//! (a part's stage, on a project with parts) that is not done. Each step is
//! told once per project and part (`project_process_step_arrived`), so
//! unchecking a step and checking it again tells nobody twice.
//!
//! Run after a step is checked off and after a move, and whenever projects
//! are loaded (`auto::run`), which catches steps that came up some other way
//! (a new project, a job finished on the schedule).

use super::schemas::Project;
use crate::phone::crm_push::push_notification_logged;
use crate::state::AppState;
use lambda_http::tracing;
use sqlx::{MySql, MySqlConnection, QueryBuilder};

/// The bell: a step you are told about is up next on a project.
pub const STEP_NEXT: &str = "project_step_next";

/// Tells the people of the steps that came up on these projects, not
/// `actor_id` (the one who made them come up). Logged, never returned.
///
/// Call it holding no connection: the pool has one.
pub async fn announce(state: &AppState, actor_id: Option<i32>, project_ids: &[i32]) {
    if let Err(error) = run(state, actor_id, project_ids).await {
        tracing::error!(?error, ?project_ids, "project step arrival failed");
    }
}

async fn run(state: &AppState, actor_id: Option<i32>, project_ids: &[i32]) -> sqlx::Result<()> {
    let Some(first) = project_ids.first() else {
        return Ok(());
    };
    let mut conn = state.pool.acquire().await?;
    let company_id: Option<i32> =
        sqlx::query_scalar("SELECT company_id FROM projects WHERE id = ?")
            .bind(first)
            .fetch_optional(&mut *conn)
            .await?;
    let Some(company_id) = company_id else {
        return Ok(());
    };
    // The first run for a company notes what is up on every project quietly:
    // only steps that come up afterwards are told.
    let started: Option<i32> = sqlx::query_scalar(
        "SELECT company_id FROM project_step_arrivals_started WHERE company_id = ?",
    )
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?;
    let quiet = started.is_none();
    let mut sql = QueryBuilder::<MySql>::new(format!(
        "{} WHERE p.company_id = ",
        super::service::project_select()
    ));
    sql.push_bind(company_id);
    if !quiet {
        crate::sql::push_in(&mut sql, "p.id", project_ids);
    }
    let projects: Vec<Project> = sql.build_query_as().fetch_all(&mut *conn).await?;
    let projects: Vec<(i32, &'static str)> = projects
        .iter()
        .filter(|p| {
            p.archived_at.is_none() && p.cancelled_at.is_none() && p.sale_cancelled_at.is_none()
        })
        .map(|p| (p.id, p.stage_code.as_str()))
        .collect();
    let steps: Vec<(i32, String)> = sqlx::query_as(
        "SELECT id, CAST(stage_code AS CHAR) FROM project_process_steps
          WHERE company_id = ?
          ORDER BY FIELD(stage_code, 'new', 'templated', 'fabrication', 'installed'), position, id",
    )
    .bind(company_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut came_up: Vec<(i32, i32, i32)> = Vec::new();
    if !projects.is_empty() && !steps.is_empty() {
        let (tracks, done) = tracks(&mut conn, company_id, &projects).await?;
        for (project_id, part_id, stage) in tracks {
            let next = steps.iter().find(|(step_id, step_stage)| {
                step_stage == stage && !done.contains(&(project_id, part_id, *step_id))
            });
            let Some((step_id, _)) = next else {
                continue;
            };
            let added = sqlx::query(
                "INSERT IGNORE INTO project_process_step_arrived (project_id, part_id, step_id)
                 VALUES (?, ?, ?)",
            )
            .bind(project_id)
            .bind(part_id)
            .bind(step_id)
            .execute(&mut *conn)
            .await?
            .rows_affected();
            if added > 0 && !quiet {
                came_up.push((project_id, part_id, *step_id));
            }
        }
    }
    if quiet {
        sqlx::query("INSERT IGNORE INTO project_step_arrivals_started (company_id) VALUES (?)")
            .bind(company_id)
            .execute(&mut *conn)
            .await?;
    }
    drop(conn);
    for (project_id, part_id, step_id) in came_up {
        tell(state, actor_id, project_id, part_id, step_id).await?;
    }
    Ok(())
}

/// Each project's (or part's) stage, and the steps they have done. A part
/// that is not fabricated sits in In Fabrication only to wait for its
/// installation: no steps of its own.
#[allow(clippy::type_complexity)]
async fn tracks(
    conn: &mut MySqlConnection,
    company_id: i32,
    projects: &[(i32, &'static str)],
) -> sqlx::Result<(Vec<(i32, i32, &'static str)>, Vec<(i32, i32, i32)>)> {
    let ids: Vec<i32> = projects.iter().map(|p| p.0).collect();
    let mut sql = QueryBuilder::<MySql>::new(
        "SELECT project_id, part_id, step_id FROM project_process_steps_done WHERE 1 = 1",
    );
    crate::sql::push_in(&mut sql, "project_id", &ids);
    let done: Vec<(i32, i32, i32)> = sql.build_query_as().fetch_all(&mut *conn).await?;
    let parts = super::parts::parts_of(conn, company_id, &ids).await?;
    let mut tracks: Vec<(i32, i32, &'static str)> = Vec::new();
    for (project_id, stage) in projects {
        let own: Vec<_> = parts
            .iter()
            .filter(|part| part.project_id == *project_id)
            .collect();
        if own.is_empty() {
            tracks.push((*project_id, 0, stage));
        }
        for part in own {
            let part_stage = part.stage_code.as_str();
            if part_stage == "fabrication" && !part.uses_fabrication {
                continue;
            }
            tracks.push((*project_id, part.id, part_stage));
        }
    }
    Ok((tracks, done))
}

/// Tells the step's people and, when the step says so, the project's sales rep.
async fn tell(
    state: &AppState,
    actor_id: Option<i32>,
    project_id: i32,
    part_id: i32,
    step_id: i32,
) -> sqlx::Result<()> {
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
    for person in people {
        // Not who was just told the step is theirs ("Gave You a Step").
        let added = sqlx::query(
            "INSERT INTO notifications
                (user_id, project_id, message, notification_type, due_at)
             SELECT ?, ?, CONCAT('Up next: ', m.message), ?, NOW()
               FROM (SELECT LEFT(CONCAT(st.name,
                                        COALESCE((SELECT CONCAT(' (', pp.name, ')')
                                                    FROM project_parts pp WHERE pp.id = ?), '')),
                                 246) AS message
                       FROM project_process_steps st WHERE st.id = ?) m
              WHERE NOT EXISTS (SELECT 1 FROM notifications n
                                 WHERE n.user_id = ? AND n.project_id = ?
                                   AND n.notification_type = ? AND n.message = m.message
                                   AND n.is_done = 0)",
        )
        .bind(person)
        .bind(project_id)
        .bind(STEP_NEXT)
        .bind(part_id)
        .bind(step_id)
        .bind(person)
        .bind(project_id)
        .bind(super::lists::STEP_ASSIGNED)
        .execute(&state.pool)
        .await?;
        if added.rows_affected() > 0 {
            push_notification_logged(
                state,
                i64::try_from(added.last_insert_id()).unwrap_or_default(),
            )
            .await;
        }
    }
    Ok(())
}
