//! Process steps that check themselves off (Admin > Projects > Auto).
//!
//! Whenever projects are loaded (the board, a project's page, payments), each
//! project's first steps not done in its stage are tested in order: a step with
//! a rule whose action happened after the rule was set, and after the step
//! before it was done, is checked off as "Auto" and its people are told. The
//! first step without a rule, or whose action hasn't happened, stops the walk.
//! A step a person unchecked stays unchecked (`project_process_step_auto_off`).
//!
//! Once a project has left a stage (the template maker's or installer's
//! Completed, Fabricated, its installed or paid date, or a move by hand), that
//! stage's steps whose work is booking, finishing or paying for the job are
//! checked off when it happened, and those that wait for the move on when it
//! moved after their rule was set (as `process::tell_moved_on` does). Work
//! done before a rule was set is checked off quietly.

use super::access::{PROJECT_EVENTS_TABLE, Stage};
use super::schemas::Project;
use crate::routes::sms::provider::{SmsProvider, phone10};
use crate::state::AppState;
use chrono::{Duration, NaiveDateTime, Utc};
use lambda_http::tracing;
use sqlx::{FromRow, MySql, MySqlConnection, QueryBuilder};

/// Rules whose work can move the project on (the template or installation
/// booked or done, a payment): still checked off once the project has left
/// their stage.
const JOB_TRIGGERS: [&str; 6] = [
    "template_booked",
    "template_completed",
    "install_booked",
    "install_completed",
    "payment_any",
    "payment_full",
];

#[derive(FromRow)]
struct Rule {
    id: i32,
    stage_code: String,
    auto_trigger: Option<String>,
    auto_ref_id: Option<i32>,
    auto_ref_ids: Option<String>,
    auto_text: Option<String>,
    auto_days: Option<i32>,
    auto_since: Option<NaiveDateTime>,
    auto_moves: bool,
}

/// Checks off what the rules say for these projects. Logged, never returned:
/// the page still loads.
///
/// Call it holding no connection: the pool has one, so a caller that keeps
/// its own waits out the acquire timeout here and nothing is checked.
pub async fn run(state: &AppState, company_id: i32, projects: &[Project]) {
    if let Err(error) = check(state, company_id, projects).await {
        tracing::error!(?error, company_id, "project step automation failed");
    }
}

async fn check(state: &AppState, company_id: i32, projects: &[Project]) -> sqlx::Result<()> {
    let live: Vec<&Project> = projects
        .iter()
        .filter(|p| {
            p.archived_at.is_none() && p.cancelled_at.is_none() && p.sale_cancelled_at.is_none()
        })
        .collect();
    if live.is_empty() {
        return Ok(());
    }
    let mut conn = state.pool.acquire().await?;
    let rules: Vec<Rule> = sqlx::query_as(
        "SELECT id, stage_code, auto_trigger, auto_ref_id, auto_ref_ids, auto_text, auto_days, auto_since,
                auto_moves <> 0 AS auto_moves
           FROM project_process_steps
          WHERE company_id = ?
          ORDER BY FIELD(stage_code, 'new', 'templated', 'fabrication', 'installed'), position, id",
    )
    .bind(company_id)
    .fetch_all(&mut *conn)
    .await?;
    if rules.iter().all(|rule| rule.auto_trigger.is_none()) {
        return Ok(());
    }
    let ids: Vec<i32> = live.iter().map(|p| p.id).collect();
    // Done once the project has moved on from the rule's stage.
    let waits_for_move =
        |rule: &Rule| rule.auto_moves || rule.auto_trigger.as_deref() == Some("stage_moved");
    let moves = if rules.iter().any(waits_for_move) {
        last_moves(&mut conn, &ids).await?
    } else {
        Vec::new()
    };
    let mut sql = QueryBuilder::<MySql>::new(
        "SELECT project_id, part_id, step_id, CAST(done_at AS DATETIME(3)) FROM project_process_steps_done WHERE 1 = 1",
    );
    crate::sql::push_in(&mut sql, "project_id", &ids);
    let done: Vec<(i32, i32, i32, NaiveDateTime)> =
        sql.build_query_as().fetch_all(&mut *conn).await?;
    let mut sql = QueryBuilder::<MySql>::new(
        "SELECT project_id, part_id, step_id FROM project_process_step_auto_off WHERE 1 = 1",
    );
    crate::sql::push_in(&mut sql, "project_id", &ids);
    let off: Vec<(i32, i32, i32)> = sql.build_query_as().fetch_all(&mut *conn).await?;
    let parts = super::parts::parts_of(&mut conn, company_id, &ids).await?;
    let now = Utc::now().naive_utc();
    // Told once the connection is back in the pool: telling needs it.
    let mut checked: Vec<(i32, i32, i32)> = Vec::new();
    let mut moved: Vec<(i32, i64)> = Vec::new();

    // A project with parts walks each part's stage (part 0: a project without),
    // with the stages it goes through (a part's type may skip some).
    let mut tracks: Vec<(&Project, i32, &str, Vec<Stage>)> = Vec::new();
    for project in live {
        let own: Vec<_> = parts
            .iter()
            .filter(|part| part.project_id == project.id)
            .collect();
        if own.is_empty() {
            tracks.push((project, 0, project.stage_code.as_str(), Stage::ALL.to_vec()));
        }
        for part in own {
            let uses: Vec<Stage> = Stage::ALL
                .into_iter()
                .filter(|stage| match stage {
                    Stage::New => part.uses_template,
                    Stage::Templated => part.uses_template || part.uses_fabrication,
                    Stage::Fabrication => part.uses_fabrication,
                    Stage::Installed | Stage::Completed => true,
                })
                .collect();
            // A part that is not fabricated only waits there for its
            // installation: the fabrication steps are not its own.
            if part.stage_code == Stage::Fabrication && !part.uses_fabrication {
                tracks.push((project, part.id, "", uses));
                continue;
            }
            tracks.push((project, part.id, part.stage_code.as_str(), uses));
        }
    }
    for (project, part, stage, uses) in tracks {
        // The stages it has left (the template or installation done, paid, or
        // moved on by hand or by itself): their steps whose work is booking,
        // finishing or paying for the job are done once it happened; those
        // that wait for the move on, when it moved after the rule was set (a
        // rule set later is not applied to old moves).
        let at_stage = parts.iter().find(|p| p.id == part).map_or_else(
            || Stage::try_from(stage.to_string()).ok(),
            |p| Some(p.stage_code),
        );
        let last_move = moves
            .iter()
            .find(|(id, _)| *id == project.id)
            .and_then(|(_, at)| *at);
        for rule in &rules {
            let (Some(at_stage), Ok(left), Some(since)) = (
                at_stage,
                Stage::try_from(rule.stage_code.clone()),
                rule.auto_since,
            ) else {
                continue;
            };
            if left >= at_stage
                || !uses.contains(&left)
                || off.contains(&(project.id, part, rule.id))
                || done
                    .iter()
                    .any(|(p, d, s, _)| *p == project.id && *d == part && *s == rule.id)
            {
                continue;
            }
            let at = if waits_for_move(rule) {
                // Completed is seconds only: the same second as the rule counts.
                last_move
                    .filter(|moved| *moved + Duration::seconds(1) > since)
                    .map(|_| now)
            } else if let Some(trigger) = rule
                .auto_trigger
                .as_deref()
                .filter(|trigger| JOB_TRIGGERS.contains(trigger))
            {
                happened(&mut conn, company_id, project, rule, trigger, since)
                    .await?
                    .map(|at| at.min(now))
            } else {
                None
            };
            let Some(at) = at else {
                continue;
            };
            let added = sqlx::query(
                "INSERT IGNORE INTO project_process_steps_done
                    (project_id, step_id, part_id, done_by, done_at, auto)
                 VALUES (?, ?, ?, NULL, ?, 1)",
            )
            .bind(project.id)
            .bind(rule.id)
            .bind(part)
            .bind(at)
            .execute(&mut *conn)
            .await?
            .rows_affected();
            // Work done before the rule was set is checked off quietly: no one
            // is told about an old job, and its schedule is left alone.
            if added > 0 && at > since {
                super::step_events::apply(&mut conn, None, project.id, rule.id, part).await;
                checked.push((project.id, rule.id, part));
            }
        }
        if stage.is_empty() {
            continue;
        }
        let mut before: Option<NaiveDateTime> = None;
        for rule in rules.iter().filter(|rule| rule.stage_code == stage) {
            if let Some((_, _, _, at)) = done
                .iter()
                .find(|(p, d, s, _)| *p == project.id && *d == part && *s == rule.id)
            {
                before = Some(*at);
                continue;
            }
            let (Some(trigger), Some(since)) = (rule.auto_trigger.as_deref(), rule.auto_since)
            else {
                break;
            };
            if off.contains(&(project.id, part, rule.id)) {
                break;
            }
            let after = before.map_or(since, |at| at.max(since));
            let happened = if trigger == "days_after" {
                let start = before.or_else(|| started(project)).unwrap_or(now);
                let at = start + Duration::days(i64::from(rule.auto_days.unwrap_or(1)));
                (at <= now).then_some(at)
            } else {
                happened(&mut conn, company_id, project, rule, trigger, after).await?
            };
            let Some(at) = happened.map(|at| at.min(now)) else {
                break;
            };
            // A rule that moves the project on (Awaiting Template, Templated)
            // moves it instead of checking the step off; the move checks it off
            // (`tell_moved_on`). Not moved (on hold, no transaction yet): it
            // waits and is tried again.
            if rule.auto_moves && matches!(stage, "new" | "templated") {
                if let Ok(from) = Stage::try_from(stage.to_string())
                    && let Some(seen) = advance(&mut conn, company_id, project, part, from).await?
                {
                    moved.push((project.id, seen));
                }
                break;
            }
            let added = sqlx::query(
                "INSERT IGNORE INTO project_process_steps_done
                    (project_id, step_id, part_id, done_by, done_at, auto)
                 VALUES (?, ?, ?, NULL, ?, 1)",
            )
            .bind(project.id)
            .bind(rule.id)
            .bind(part)
            .bind(at)
            .execute(&mut *conn)
            .await?
            .rows_affected();
            if added > 0 {
                super::step_events::apply(&mut conn, None, project.id, rule.id, part).await;
                checked.push((project.id, rule.id, part));
            }
            before = Some(at);
        }
    }
    drop(conn);
    for (project_id, seen) in moved {
        super::process::tell_moved_on(state, None, Some("Auto"), company_id, project_id, seen)
            .await;
    }
    for (project_id, step_id, part_id) in checked {
        super::process::tell_step_people(state, None, Some("Auto"), project_id, step_id, part_id)
            .await;
    }
    Ok(())
}

/// Moves a project (or one of its parts) on from `from`: Awaiting Template to
/// Templated, Templated to In Fabrication. Not for a project on hold, one moved
/// back by hand, or fabrication before its transaction is linked. The stage
/// history id from before the move, or none when nothing moved.
async fn advance(
    conn: &mut MySqlConnection,
    company_id: i32,
    project: &Project,
    part: i32,
    from: Stage,
) -> sqlx::Result<Option<i64>> {
    let to = match from {
        Stage::New => Stage::Templated,
        Stage::Templated => Stage::Fabrication,
        _ => return Ok(None),
    };
    if project.is_on_hold || (to == Stage::Fabrication && project.sale_id.is_none()) {
        return Ok(None);
    }
    let seen: i64 = sqlx::query_scalar(
        "SELECT CAST(COALESCE(MAX(id), 0) AS SIGNED) FROM project_stage_history WHERE project_id = ?",
    )
    .bind(project.id)
    .fetch_one(&mut *conn)
    .await?;
    let changed = if part > 0 {
        sqlx::query(
            "UPDATE project_parts pp
               JOIN project_part_types pt ON pt.id = pp.type_id
                SET pp.stage_code = ?
              WHERE pp.id = ? AND pp.project_id = ? AND pp.stage_code = ?
                AND pp.stage_cap IS NULL AND (? <> 'fabrication' OR pt.uses_fabrication = 1)",
        )
        .bind(to.as_str())
        .bind(part)
        .bind(project.id)
        .bind(from.as_str())
        .bind(to.as_str())
        .execute(&mut *conn)
        .await?
        .rows_affected()
    } else {
        sqlx::query(
            "UPDATE projects SET stage_code = ? WHERE id = ? AND company_id = ? AND stage_cap IS NULL",
        )
        .bind(to.as_str())
        .bind(project.id)
        .bind(company_id)
        .execute(&mut *conn)
        .await?
        .rows_affected()
    };
    if changed == 0 {
        return Ok(None);
    }
    sqlx::query(
        "UPDATE projects SET version = version + 1,
                templated_at = IF(? = 'templated', COALESCE(templated_at, UTC_TIMESTAMP()), templated_at)
          WHERE id = ? AND company_id = ?",
    )
    .bind(to.as_str())
    .bind(project.id)
    .bind(company_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO project_stage_history
            (project_id, company_id, from_stage, to_stage, actor_id, source_action, part_id)
         SELECT id, company_id, ?, ?, created_by, 'auto', NULLIF(?, 0) FROM projects WHERE id = ?",
    )
    .bind(from.as_str())
    .bind(to.as_str())
    .bind(part)
    .bind(project.id)
    .execute(&mut *conn)
    .await?;
    Ok(Some(seen))
}

/// When the project started (`created_at`, UTC ISO).
fn started(project: &Project) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(&project.created_at, "%Y-%m-%dT%H:%M:%SZ").ok()
}

/// When each project last moved, by hand or by itself (UTC): its newest stage
/// history row, a template or installation its crew marked Completed, marked
/// fabricated, or its installed or paid date. None: it never moved.
async fn last_moves(
    conn: &mut MySqlConnection,
    ids: &[i32],
) -> sqlx::Result<Vec<(i32, Option<NaiveDateTime>)>> {
    const NEVER: &str = "TIMESTAMP('1970-01-01')";
    let mut sql = QueryBuilder::<MySql>::new(format!(
        "SELECT p.id, CAST(NULLIF(GREATEST(
                COALESCE((SELECT MAX(h.created_at) FROM project_stage_history h
                           WHERE h.project_id = p.id), {NEVER}),
                COALESCE((SELECT MAX(e.completed_at) FROM {PROJECT_EVENTS_TABLE} e
                            JOIN calendars cal ON cal.id = e.calendar_id
                                              AND cal.slug IN ('templates', 'installations')
                           WHERE e.crew_status = 'completed' AND e.deleted_date IS NULL), {NEVER}),
                COALESCE(p.fabricated_at, {NEVER}),
                COALESCE((SELECT MAX(pp.fabricated_at) FROM project_parts pp
                           WHERE pp.project_id = p.id), {NEVER}),
                COALESCE(s.installed_date, {NEVER}),
                COALESCE(s.paid_date, {NEVER}),
                COALESCE(p.qbo_paid_at, {NEVER})), {NEVER}) AS DATETIME(3))
           FROM projects p LEFT JOIN sales s ON s.id = p.sale_id
          WHERE 1 = 1"
    ));
    crate::sql::push_in(&mut sql, "p.id", ids);
    sql.build_query_as().fetch_all(&mut *conn).await
}

/// When the rule's action first happened for the project after `after`.
async fn happened(
    conn: &mut MySqlConnection,
    company_id: i32,
    project: &Project,
    rule: &Rule,
    trigger: &str,
    after: NaiveDateTime,
) -> sqlx::Result<Option<NaiveDateTime>> {
    let phone = project
        .customer_phone
        .as_deref()
        .map(phone10)
        .filter(|phone| phone.len() == 10)
        .unwrap_or_default();
    let text = rule.auto_text.clone().unwrap_or_default();
    let at: Option<NaiveDateTime> = match trigger {
        "email_template" | "text_template" => {
            sqlx::query_scalar(
                "SELECT CAST(MIN(st.sent_at) AS DATETIME(3)) FROM sent_templates st
                  WHERE st.company_id = ? AND st.kind = ? AND FIND_IN_SET(st.template_id, ?) > 0 AND st.sent_at > ?
                    AND (st.deal_id = ? OR st.customer_id = ?
                         OR (st.phone10 IS NOT NULL AND st.phone10 = ?)
                         OR (st.email IS NOT NULL AND st.email IN
                             (SELECT LOWER(TRIM(ce.email)) FROM customers_emails ce WHERE ce.customer_id = ?)))",
            )
            .bind(company_id)
            .bind(if trigger == "email_template" { "email" } else { "sms" })
            .bind(
                rule.auto_ref_ids
                    .clone()
                    .filter(|ids| !ids.is_empty())
                    .unwrap_or_else(|| rule.auto_ref_id.unwrap_or(0).to_string()),
            )
            .bind(after)
            .bind(project.source_deal_id)
            .bind(project.customer_id)
            .bind(&phone)
            .bind(project.customer_id)
            .fetch_one(&mut *conn)
            .await?
        }
        "customer_replied" => {
            let email: Option<NaiveDateTime> = sqlx::query_scalar(
                "SELECT CAST(MIN(e.sent_at) AS DATETIME(3)) FROM emails e
                  WHERE e.deleted_at IS NULL AND e.sender_user_id IS NULL AND e.sent_at > ?
                    AND (e.company_id = ? OR e.company_id IS NULL)
                    AND (e.deal_id = ?
                         OR EXISTS (SELECT 1 FROM email_participants pc
                                     WHERE pc.email_id = e.id AND pc.type = 'from' AND pc.customer_id = ?)
                         OR LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX(e.sender_email, '<', -1), '>', 1)))
                            IN (SELECT LOWER(TRIM(ce.email)) FROM customers_emails ce WHERE ce.customer_id = ?))",
            )
            .bind(after)
            .bind(company_id)
            .bind(project.source_deal_id)
            .bind(project.customer_id)
            .bind(project.customer_id)
            .fetch_one(&mut *conn)
            .await?;
            let mut first = email;
            if !phone.is_empty() {
                for table in SmsProvider::ALL.map(SmsProvider::sms_table) {
                    let text: Option<NaiveDateTime> =
                        sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                            "SELECT CAST(MIN(created_date) AS DATETIME(3)) FROM {table}
                              WHERE company_id = ? AND direction = 'inbound' AND created_date > ?
                                AND (phone_digits = ? OR sender10 = ?)"
                        )))
                        .bind(company_id)
                        .bind(after)
                        .bind(&phone)
                        .bind(&phone)
                        .fetch_one(&mut *conn)
                        .await?;
                    first = match (first, text) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    };
                }
            }
            first
        }
        "file_added" => {
            sqlx::query_scalar(
                "SELECT CAST(MIN(at) AS DATETIME(3)) FROM (
                   SELECT created_at AS at FROM project_files
                    WHERE project_id = ? AND deleted_at IS NULL AND created_at > ?
                   UNION ALL
                   SELECT created_at FROM deals_documents WHERE deal_id = ? AND created_at > ?
                 ) added",
            )
            .bind(project.id)
            .bind(after)
            .bind(project.source_deal_id)
            .bind(after)
            .fetch_one(&mut *conn)
            .await?
        }
        "sent_to_sign" | "signed" => {
            let (status, at) = if trigger == "signed" {
                ("sr.status = 'signed'", "sr.updated_at")
            } else {
                ("sr.status <> 'draft'", "COALESCE(e.sent_at, sr.created_at)")
            };
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT CAST(MIN({at}) AS DATETIME(3)) FROM signature_requests sr
                   LEFT JOIN emails e ON e.id = sr.email_id
                  WHERE sr.company_id = ? AND {status}
                    AND (sr.customer_id = ? OR sr.deal_id = ?) AND {at} > ?
                    AND (? = '' OR sr.filename LIKE CONCAT('%', ?, '%'))"
            )))
            .bind(company_id)
            .bind(project.customer_id)
            .bind(project.source_deal_id)
            .bind(after)
            .bind(&text)
            .bind(&text)
            .fetch_one(&mut *conn)
            .await?
        }
        "questionnaire_filled" => {
            sqlx::query_scalar(
                "SELECT CAST(MIN(created_at) AS DATETIME(3)) FROM sales_questionnaire_responses
                  WHERE deal_id = ? AND created_at > ? AND TRIM(COALESCE(answer, '')) <> ''",
            )
            .bind(project.source_deal_id)
            .bind(after)
            .fetch_one(&mut *conn)
            .await?
        }
        "template_booked" | "install_booked" => {
            // On the calendar now counts, even if it was booked before.
            sqlx::query_scalar(
                "SELECT CAST(MIN(GREATEST(e.created_date, ?)) AS DATETIME(3)) FROM events e
                   JOIN calendars cal ON cal.id = e.calendar_id AND cal.slug = ? AND cal.company_id = ?
                  WHERE e.deleted_date IS NULL AND e.unscheduled = 0
                    AND (e.project_id = ?
                         OR (e.project_id IS NULL AND e.customer_id = ?
                             AND e.created_date >= (SELECT created_at FROM projects WHERE id = ?) - INTERVAL 1 DAY))",
            )
            .bind(after)
            .bind(if trigger == "template_booked" { "templates" } else { "installations" })
            .bind(company_id)
            .bind(project.id)
            .bind(project.customer_id)
            .bind(project.id)
            .fetch_one(&mut *conn)
            .await?
        }
        "template_completed" | "install_completed" => {
            // Done now counts, even if it was done before (as booked): the
            // crew's Completed, or for the installation the transaction's
            // installed date.
            let job: Option<NaiveDateTime> = sqlx::query_scalar(
                "SELECT CAST(MIN(GREATEST(e.completed_at, ?)) AS DATETIME(3)) FROM events e
                   JOIN calendars cal ON cal.id = e.calendar_id AND cal.slug = ? AND cal.company_id = ?
                  WHERE e.deleted_date IS NULL AND e.crew_status = 'completed'
                    AND e.completed_at IS NOT NULL
                    AND (e.project_id = ?
                         OR (e.project_id IS NULL AND e.customer_id = ?
                             AND e.created_date >= (SELECT created_at FROM projects WHERE id = ?) - INTERVAL 1 DAY))",
            )
            .bind(after)
            .bind(if trigger == "template_completed" { "templates" } else { "installations" })
            .bind(company_id)
            .bind(project.id)
            .bind(project.customer_id)
            .bind(project.id)
            .fetch_one(&mut *conn)
            .await?;
            let installed: Option<NaiveDateTime> = if trigger == "install_completed" {
                sqlx::query_scalar(
                    "SELECT CAST(GREATEST(s.installed_date, ?) AS DATETIME(3)) FROM projects p
                       JOIN sales s ON s.id = p.sale_id
                      WHERE p.id = ? AND s.installed_date IS NOT NULL",
                )
                .bind(after)
                .bind(project.id)
                .fetch_optional(&mut *conn)
                .await?
                .flatten()
            } else {
                None
            };
            match (job, installed) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            }
        }
        "invoice_sent" => {
            sqlx::query_scalar(
                "SELECT CAST(MIN(sent_at) AS DATETIME(3)) FROM invoices
                  WHERE company_id = ? AND customer_id = ? AND sent_at > ?",
            )
            .bind(company_id)
            .bind(project.customer_id)
            .bind(after)
            .fetch_one(&mut *conn)
            .await?
        }
        "payment_any" | "payment_full" => {
            // Paid now counts, even if it was paid before.
            let paid = if trigger == "payment_full" {
                "COALESCE(p.qbo_paid_at, s.paid_date)"
            } else {
                "COALESCE(p.qbo_first_paid_at, p.qbo_paid_at, s.paid_date)"
            };
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT CAST(GREATEST({paid}, ?) AS DATETIME(3)) FROM projects p
                   LEFT JOIN sales s ON s.id = p.sale_id
                  WHERE p.id = ? AND {paid} IS NOT NULL"
            )))
            .bind(after)
            .bind(project.id)
            .fetch_optional(&mut *conn)
            .await?
            .flatten()
        }
        "task_done" => {
            sqlx::query_scalar(
                "SELECT CAST(MIN(completed_at) AS DATETIME(3)) FROM project_activities
                  WHERE project_id = ? AND deleted_at IS NULL AND completed_at > ?
                    AND name LIKE CONCAT('%', ?, '%')",
            )
            .bind(project.id)
            .bind(after)
            .bind(&text)
            .fetch_one(&mut *conn)
            .await?
        }
        // `stage_moved`: the move itself checks it off (`process::tell_moved_on`).
        _ => None,
    };
    Ok(at)
}

/// Remembers an email or text sent from a template, for steps that check off
/// when one goes to the project's customer (best effort).
#[allow(clippy::too_many_arguments)]
pub async fn log_sent_template(
    pool: &sqlx::MySqlPool,
    company_id: i32,
    kind: &str,
    template_id: Option<i32>,
    deal_id: Option<i32>,
    customer_id: Option<i32>,
    phone: Option<&str>,
    email: Option<&str>,
) {
    let Some(template_id) = template_id.filter(|id| *id > 0) else {
        return;
    };
    let result = sqlx::query(
        "INSERT INTO sent_templates (company_id, kind, template_id, deal_id, customer_id, phone10, email)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(company_id)
    .bind(kind)
    .bind(template_id)
    .bind(deal_id.filter(|id| *id > 0))
    .bind(customer_id.filter(|id| *id > 0))
    .bind(phone.map(phone10).filter(|phone| phone.len() == 10))
    .bind(email.map(|e| e.trim().to_lowercase()).filter(|e| !e.is_empty()))
    .execute(pool)
    .await;
    if let Err(error) = result {
        tracing::warn!(?error, "sent template not logged");
    }
}
