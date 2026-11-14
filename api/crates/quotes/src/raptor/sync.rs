//! Sends each new project of a company with a Raptor login to Raptor.
//!
//! Only projects created since the login was saved (`company.raptor_since`),
//! once each. A project is claimed in `project_raptor_jobs`, its Raptor jobs
//! are made and filled, and the row says `done` with the jobs, or `failed`
//! with the reason; a failed project is tried again 10 minutes later, three
//! times in all, and carries on from the jobs it already made.
//!
//! After that the jobs follow the CRM for 60 days: every 15 minutes a
//! project's plan is made again and compared with what was sent. A changed
//! name renames the job, changed customer / address / materials / sinks are
//! written again, and changed note lines replace those lines in the "Notes"
//! note; everything the office typed itself stays. A new full-height area
//! gets its own job. Jobs the office made or deleted are left alone.
//!
//! Sale companies (`Config::sale_companies`, Granite Depot of Indianapolis
//! for now) work from the quote's buttons instead, as test jobs ("… TEST",
//! reference "TEST"). Mark as Sold queues the deal's project (`sold`: a
//! `waiting` row, whenever the project was created) and Undo sale marks its
//! jobs for deletion (`unsold`: `removing`). Neither waits for Raptor: the
//! next run, a minute later, makes or deletes the jobs and tries again when
//! Raptor is down.

use super::client::{API, ListJob, Parts, Raptor, next_number};
use super::plan::{self, Address, Cutout, Job, Material, Plan, Source};
use crate::quote_pricing::doc::QuoteDoc;
use common::telephony::ringcentral_auth::{decrypt, env_key};
use serde::{Deserialize, Serialize};
use sqlx::{MySqlPool, Row};
use std::time::{Duration, Instant};

const MAX_ATTEMPTS: i32 = 3;
/// How often deleting an unsold project's jobs is tried.
const MAX_REMOVALS: i32 = 10;
/// How long after the project its jobs follow the CRM.
const FOLLOW_DAYS: i32 = 60;
/// How often a project's jobs are compared with the CRM.
const FOLLOW_EVERY_MINUTES: i32 = 15;

/// What test jobs end with.
const TEST: &str = " TEST";

/// Where Raptor is and the key that opens the saved passwords.
pub struct Config {
    pub api_url: String,
    pub key: Option<[u8; 32]>,
    /// Companies whose jobs follow Mark as Sold and Unsell, as test jobs.
    pub sale_companies: Vec<i32>,
}

impl Config {
    /// Raptor's API (or `RAPTOR_API_URL`) and `AES_KEY`; Granite Depot of
    /// Indianapolis (company 1) follows its sales.
    pub fn from_env() -> Self {
        Self {
            api_url: std::env::var("RAPTOR_API_URL").unwrap_or_else(|_| API.to_string()),
            key: env_key(),
            sale_companies: vec![1],
        }
    }

    fn sales(&self, company_id: i32) -> bool {
        self.sale_companies.contains(&company_id)
    }

    /// The plan as this company's jobs are named.
    fn named(&self, company_id: i32, mut plan: Plan) -> Plan {
        if self.sales(company_id) {
            for job in &mut plan.jobs {
                job.name.push_str(TEST);
            }
        }
        plan
    }
}

/// What a run did.
#[derive(Debug, Default, Serialize)]
pub struct RunSummary {
    /// Projects whose Raptor jobs are all made.
    pub done: u32,
    /// Projects that failed this time.
    pub failed: u32,
    /// Projects whose jobs were brought up to the CRM.
    pub updated: u32,
    /// Unsold projects whose jobs were deleted.
    pub removed: u32,
}

/// One Raptor job made (or found) for a project.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobRecord {
    pub name: String,
    pub reference: String,
    pub id: String,
    /// Raptor already had a job by that name; it was left as it was.
    #[serde(default)]
    pub existing: bool,
    #[serde(default)]
    pub filled: bool,
    #[serde(default)]
    pub noted: bool,
    /// The "Notes" note made.
    #[serde(default)]
    pub note_id: Option<String>,
    /// What Raptor was last given for this job.
    #[serde(default)]
    pub sent: Option<Sent>,
}

/// What a job was given, to see later what the CRM changed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Sent {
    pub name: String,
    pub customer_name: String,
    pub phone: String,
    pub address: Address,
    pub materials: Vec<Material>,
    pub cutouts: Vec<Cutout>,
    pub notes: String,
}

impl Sent {
    pub fn of(plan: &Plan, job: &Job) -> Self {
        Self {
            name: job.name.clone(),
            customer_name: plan.customer_name.clone(),
            phone: plan.phone.clone(),
            address: plan.address.clone(),
            materials: job.materials.clone(),
            cutouts: job.cutouts.clone(),
            notes: job.notes.clone(),
        }
    }
}

/// A project has one main job and maybe a full-height ("FHBS") one; a job
/// keeps its kind when it is renamed.
fn same_kind(a: &str, b: &str) -> bool {
    let fhbs = |name: &str| name.trim_end().trim_end_matches(TEST).ends_with("FHBS");
    fhbs(a) == fhbs(b)
}

struct Login {
    company_id: i32,
    email: String,
    password: String,
}

async fn logins(pool: &MySqlPool, key: Option<&[u8; 32]>) -> sqlx::Result<Vec<Login>> {
    let rows = sqlx::query(
        "SELECT id, raptor_email, raptor_password FROM company
          WHERE deleted_at IS NULL AND raptor_since IS NOT NULL
            AND COALESCE(raptor_email, '') <> '' AND COALESCE(raptor_password, '') <> ''",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let company_id: i32 = row.get(0);
            let sealed: String = row.get(2);
            let password = key.and_then(|key| decrypt(key, sealed.as_bytes()));
            if password.is_none() {
                lambda_http::tracing::warn!(
                    company_id,
                    "Raptor password does not open with AES_KEY"
                );
            }
            Some(Login {
                company_id,
                email: row.get(1),
                password: password?,
            })
        })
        .collect())
}

/// Projects of sale company `company_id` that Mark as Sold queued (or that
/// failed and are due again) and whose deal still has a sold quote.
async fn queued_projects(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Vec<i32>> {
    sqlx::query_scalar(
        "SELECT r.project_id FROM project_raptor_jobs r JOIN projects p ON p.id = r.project_id
          WHERE r.company_id = ? AND r.attempts < ?
            AND (r.status = 'waiting'
                 OR (r.status IN ('working', 'failed')
                     AND r.updated_at <= NOW() - INTERVAL 10 MINUTE))
            AND p.cancelled_at IS NULL AND p.archived_at IS NULL
            AND EXISTS (
                SELECT 1 FROM quotes q
                 WHERE q.deal_id = p.source_deal_id AND q.company_id = p.company_id
                   AND q.deleted_at IS NULL AND q.status IN ('accepted', 'converted'))
          ORDER BY r.updated_at
          LIMIT 20",
    )
    .bind(company_id)
    .bind(MAX_ATTEMPTS)
    .fetch_all(pool)
    .await
}

/// Projects of `company_id` that still need Raptor jobs.
async fn due_projects(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Vec<i32>> {
    sqlx::query_scalar(
        "SELECT p.id FROM projects p JOIN company c ON c.id = p.company_id
          WHERE p.company_id = ? AND p.created_at >= c.raptor_since
            AND p.cancelled_at IS NULL AND p.archived_at IS NULL
            AND NOT EXISTS (
                SELECT 1 FROM project_raptor_jobs r
                 WHERE r.project_id = p.id
                   AND (r.status = 'done' OR r.attempts >= ?
                        OR r.updated_at > NOW() - INTERVAL 10 MINUTE))
          ORDER BY p.id
          LIMIT 20",
    )
    .bind(company_id)
    .bind(MAX_ATTEMPTS)
    .fetch_all(pool)
    .await
}

/// Takes the project for this run; its jobs from earlier tries, or `None`
/// when another run has it or it is finished.
async fn claim(
    pool: &MySqlPool,
    project_id: i32,
    company_id: i32,
) -> sqlx::Result<Option<Vec<JobRecord>>> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT status, attempts, CAST(jobs AS CHAR),
                updated_at > NOW() - INTERVAL 10 MINUTE
           FROM project_raptor_jobs WHERE project_id = ? FOR UPDATE",
    )
    .bind(project_id)
    .fetch_optional(&mut *tx)
    .await?;
    let jobs = match row {
        None => {
            let inserted = sqlx::query(
                "INSERT IGNORE INTO project_raptor_jobs (project_id, company_id, status, attempts)
                 VALUES (?, ?, 'working', 1)",
            )
            .bind(project_id)
            .bind(company_id)
            .execute(&mut *tx)
            .await?;
            (inserted.rows_affected() == 1).then(Vec::new)
        }
        Some(row) => {
            let status: String = row.get(0);
            let attempts: i32 = row.get(1);
            let recent: bool = row.get(3);
            // A queued project goes at once; one being deleted not at all.
            let waits = recent && status != "waiting";
            if status == "done" || status == "removing" || attempts >= MAX_ATTEMPTS || waits {
                None
            } else {
                sqlx::query(
                    "UPDATE project_raptor_jobs
                        SET status = 'working', attempts = attempts + 1, error = NULL
                      WHERE project_id = ?",
                )
                .bind(project_id)
                .execute(&mut *tx)
                .await?;
                let jobs: Option<String> = row.get(2);
                Some(
                    jobs.and_then(|j| serde_json::from_str(&j).ok())
                        .unwrap_or_default(),
                )
            }
        }
    };
    tx.commit().await?;
    Ok(jobs)
}

async fn finish(
    pool: &MySqlPool,
    project_id: i32,
    jobs: &[JobRecord],
    error: Option<&str>,
) -> sqlx::Result<()> {
    let error: Option<String> = error.map(|e| e.chars().take(1000).collect());
    // Unsold meanwhile (`removing`): it stays so, with the jobs to delete.
    sqlx::query(
        "UPDATE project_raptor_jobs
            SET jobs = ?, error = ?, status = IF(status = 'working', ?, status)
          WHERE project_id = ?",
    )
    .bind(serde_json::to_string(jobs).unwrap_or_default())
    .bind(error.as_deref())
    .bind(if error.is_some() { "failed" } else { "done" })
    .bind(project_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// The project's plan from its sold quote; `Err` says why there is none.
pub async fn load_plan(
    pool: &MySqlPool,
    project_id: i32,
    company_id: i32,
) -> sqlx::Result<Result<Plan, String>> {
    let Some(project) = sqlx::query(
        "SELECT p.customer_id, CAST(p.source_deal_id AS SIGNED), p.project_address, cu.name, cu.phone,
                cu.company_name
           FROM projects p JOIN customers cu ON cu.id = p.customer_id
          WHERE p.id = ? AND p.company_id = ?",
    )
    .bind(project_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(Err("The project is gone".into()));
    };
    let customer_id: i32 = project.get(0);
    let deal_id: Option<i64> = project.get(1);
    let project_address: Option<String> = project.get(2);
    let customer_name: String = project.get(3);
    let phone: Option<String> = project.get(4);
    let company_name: Option<String> = project.get(5);
    // The deal's quote first, sold ones first; then the customer's newest.
    let Some(quote) = sqlx::query(
        "SELECT q.sales_rep, q.address, q.customer_accepted_option_id, q.price_list_revision_id,
                CAST(COALESCE(r.drawing_json, q.draft_drawing_json) AS CHAR)
           FROM quotes q
           LEFT JOIN quote_revisions r
                  ON r.quote_id = q.id AND r.revision_number = q.current_revision
          WHERE q.company_id = ? AND q.deleted_at IS NULL
            AND (q.deal_id = ? OR q.customer_id = ?)
          ORDER BY COALESCE(q.deal_id = ?, 0) DESC,
                   q.status IN ('accepted', 'converted') DESC,
                   COALESCE(q.signed_at, q.updated_at, q.created_date) DESC, q.id DESC
          LIMIT 1",
    )
    .bind(company_id)
    .bind(deal_id)
    .bind(customer_id)
    .bind(deal_id)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(Err("The project has no quote".into()));
    };
    let salesperson: Option<String> = quote.get(0);
    let address: Option<String> = quote.get(1);
    let option: Option<i32> = quote.get(2);
    let revision: Option<i32> = quote.get(3);
    let drawing: Option<String> = quote.get(4);
    let Some(doc) = drawing
        .map(serde_json::Value::String)
        .and_then(|v| QuoteDoc::parse(&v))
    else {
        return Ok(Err("The quote has no drawing".into()));
    };
    let sq_ft_up = match revision {
        Some(id) => sqlx::query_scalar::<_, Option<String>>(
            "SELECT CAST(info_json AS CHAR) FROM price_list_revisions WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(pool)
        .await?
        .flatten()
        .and_then(|info| serde_json::from_str::<serde_json::Value>(&info).ok())
        .is_some_and(|info| info["materialRounding"] == "up"),
        None => false,
    };
    let notes: Vec<String> = sqlx::query_scalar(
        "SELECT content FROM project_notes
          WHERE project_id = ? AND deleted_at IS NULL ORDER BY created_at, id",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await?;
    let address = address.filter(|a| !a.trim().is_empty()).or(project_address);
    Ok(Ok(plan::plan(&Source {
        customer_name: &customer_name,
        company_name: company_name.as_deref(),
        phone: phone.as_deref(),
        address: address.as_deref(),
        salesperson: salesperson.as_deref(),
        doc: &doc,
        option_index: option.and_then(|o| usize::try_from(o).ok()).unwrap_or(0),
        project_notes: &notes,
        sq_ft_up,
    })))
}

/// Makes the plan's jobs that `done` does not have yet; `done` grows as it goes.
async fn send(
    raptor: &Raptor,
    plan: &Plan,
    known: &mut Vec<ListJob>,
    done: &mut Vec<JobRecord>,
) -> Result<(), String> {
    for job in &plan.jobs {
        let index = if let Some(index) = done.iter().position(|r| same_kind(&r.name, &job.name)) {
            index
        } else {
            if let Some(found) = known
                .iter()
                .find(|k| k.active() && k.name.trim().eq_ignore_ascii_case(&job.name))
            {
                done.push(JobRecord {
                    name: job.name.clone(),
                    reference: found.reference.clone().unwrap_or_default(),
                    id: found.id.clone(),
                    existing: true,
                    ..JobRecord::default()
                });
                continue;
            }
            // Test jobs keep the office's numbers free.
            let reference = if job.name.ends_with(TEST) {
                TEST.trim().to_string()
            } else {
                format!("#{}", next_number(known))
            };
            let id = raptor.create_job(&reference, &job.name).await?;
            known.push(ListJob {
                id: id.clone(),
                name: job.name.clone(),
                reference: Some(reference.clone()),
                status: Some("Active".into()),
            });
            done.push(JobRecord {
                name: job.name.clone(),
                reference,
                id,
                ..JobRecord::default()
            });
            done.len() - 1
        };
        let record = &mut done[index];
        if record.existing {
            continue;
        }
        if !record.filled {
            raptor.fill(&record.id, plan, job).await?;
            record.filled = true;
        }
        if !record.noted {
            record.note_id = Some(raptor.add_note(&record.id, &job.notes).await?);
            record.noted = true;
        }
        if record.sent.is_none() {
            record.sent = Some(Sent::of(plan, job));
        }
    }
    Ok(())
}

/// Whether `done` needs Raptor to match `plan`. A job sent before jobs were
/// followed takes the plan as what it was given.
fn behind(plan: &Plan, done: &mut [JobRecord]) -> bool {
    let mut behind = false;
    for job in &plan.jobs {
        match done.iter_mut().find(|r| same_kind(&r.name, &job.name)) {
            None => behind = true,
            Some(r) if r.existing => {}
            Some(r) if !r.filled || !r.noted => behind = true,
            Some(r) => {
                let now = Sent::of(plan, job);
                match &r.sent {
                    None => r.sent = Some(now),
                    Some(sent) => behind |= *sent != now,
                }
            }
        }
    }
    behind
}

/// Brings one job from what it was given to what the CRM says now.
async fn update(
    raptor: &Raptor,
    plan: &Plan,
    job: &Job,
    record: &mut JobRecord,
) -> Result<(), String> {
    let now = Sent::of(plan, job);
    let Some(old) = record.sent.clone() else {
        record.sent = Some(now);
        return Ok(());
    };
    if old == now {
        return Ok(());
    }
    let parts = Parts {
        customer: old.customer_name != now.customer_name || old.phone != now.phone,
        address: old.address != now.address,
        materials: old.materials != now.materials,
        cutouts: old.cutouts != now.cutouts,
    };
    let rename = (old.name != now.name).then_some(now.name.as_str());
    if rename.is_some() || parts.any() {
        raptor
            .update_job(&record.id, rename, plan, job, parts)
            .await?;
    }
    if old.notes != now.notes {
        raptor
            .update_note(
                &record.id,
                record.note_id.as_deref(),
                &old.notes,
                &now.notes,
            )
            .await?;
    }
    record.name.clone_from(&now.name);
    record.sent = Some(now);
    Ok(())
}

/// Makes what is missing, then updates each job of ours Raptor still has.
async fn follow(
    raptor: &Raptor,
    plan: &Plan,
    known: &mut Vec<ListJob>,
    done: &mut Vec<JobRecord>,
) -> Result<(), String> {
    send(raptor, plan, known, done).await?;
    for job in &plan.jobs {
        let Some(record) = done.iter_mut().find(|r| same_kind(&r.name, &job.name)) else {
            continue;
        };
        let deleted = known.iter().any(|k| k.id == record.id && !k.active());
        if !record.existing && !deleted {
            update(raptor, plan, job, record).await?;
        }
    }
    Ok(())
}

/// Sent projects of `company_id` whose jobs are due a look.
async fn followed_projects(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Vec<i32>> {
    sqlx::query_scalar(
        "SELECT r.project_id FROM project_raptor_jobs r JOIN projects p ON p.id = r.project_id
          WHERE r.company_id = ? AND r.status = 'done'
            AND p.cancelled_at IS NULL AND p.archived_at IS NULL
            AND p.created_at > NOW() - INTERVAL ? DAY
            AND r.updated_at < NOW() - INTERVAL ? MINUTE
          ORDER BY r.updated_at
          LIMIT 25",
    )
    .bind(company_id)
    .bind(FOLLOW_DAYS)
    .bind(FOLLOW_EVERY_MINUTES)
    .fetch_all(pool)
    .await
}

/// Takes a sent project for this look; its jobs, or `None` when another run
/// has it.
async fn claim_followed(pool: &MySqlPool, project_id: i32) -> sqlx::Result<Option<Vec<JobRecord>>> {
    let taken = sqlx::query(
        "UPDATE project_raptor_jobs SET updated_at = NOW()
          WHERE project_id = ? AND status = 'done' AND updated_at < NOW() - INTERVAL ? MINUTE",
    )
    .bind(project_id)
    .bind(FOLLOW_EVERY_MINUTES)
    .execute(pool)
    .await?;
    if taken.rows_affected() != 1 {
        return Ok(None);
    }
    let jobs: Option<String> = sqlx::query_scalar(
        "SELECT CAST(jobs AS CHAR) FROM project_raptor_jobs WHERE project_id = ?",
    )
    .bind(project_id)
    .fetch_one(pool)
    .await?;
    Ok(Some(
        jobs.and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default(),
    ))
}

/// Saves a followed project's jobs; it stays `done`, with the error if any.
async fn save_followed(
    pool: &MySqlPool,
    project_id: i32,
    jobs: &[JobRecord],
    error: Option<&str>,
) -> sqlx::Result<()> {
    let error: Option<String> = error.map(|e| e.chars().take(1000).collect());
    sqlx::query("UPDATE project_raptor_jobs SET jobs = ?, error = ? WHERE project_id = ?")
        .bind(serde_json::to_string(jobs).unwrap_or_default())
        .bind(error)
        .bind(project_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// A signed-in Raptor and its job list.
struct Session {
    raptor: Raptor,
    known: Vec<ListJob>,
}

async fn open(config: &Config, login: &Login) -> Result<Session, String> {
    let http = common::http::client();
    let raptor = Raptor::login(http, &config.api_url, &login.email, &login.password).await?;
    let known = raptor.jobs().await?;
    Ok(Session { raptor, known })
}

/// Sends the due projects of every company with a Raptor login and keeps
/// the sent ones up to the CRM, until `budget` is spent.
pub async fn run(pool: &MySqlPool, budget: Duration, config: &Config) -> sqlx::Result<RunSummary> {
    let started = Instant::now();
    let mut summary = RunSummary::default();
    for login in logins(pool, config.key.as_ref()).await? {
        let mut session: Option<Session> = None;
        let sales = config.sales(login.company_id);
        if sales && !remove_unsold(pool, config, &login, &mut session, &mut summary).await? {
            continue;
        }
        let due = if sales {
            queued_projects(pool, login.company_id).await?
        } else {
            due_projects(pool, login.company_id).await?
        };
        if !due.is_empty() && session.is_none() {
            match open(config, &login).await {
                Ok(open) => session = Some(open),
                Err(error) => {
                    lambda_http::tracing::warn!(company_id = login.company_id, %error, "Raptor login failed");
                    continue;
                }
            }
        }
        if let Some(Session { raptor, known }) = session.as_mut() {
            for project_id in due {
                if started.elapsed() > budget {
                    return Ok(summary);
                }
                new_project(
                    pool,
                    config,
                    &login,
                    project_id,
                    raptor,
                    known,
                    &mut summary,
                )
                .await?;
            }
        }
        for project_id in followed_projects(pool, login.company_id).await? {
            if started.elapsed() > budget {
                return Ok(summary);
            }
            let Some(mut done) = claim_followed(pool, project_id).await? else {
                continue;
            };
            let Ok(plan) = load_plan(pool, project_id, login.company_id).await? else {
                continue;
            };
            let plan = config.named(login.company_id, plan);
            if !behind(&plan, &mut done) {
                save_followed(pool, project_id, &done, None).await?;
                continue;
            }
            if session.is_none() {
                match open(config, &login).await {
                    Ok(open) => session = Some(open),
                    Err(error) => {
                        lambda_http::tracing::warn!(company_id = login.company_id, %error, "Raptor login failed");
                        save_followed(pool, project_id, &done, Some(&error)).await?;
                        break;
                    }
                }
            }
            let Some(Session { raptor, known }) = session.as_mut() else {
                break;
            };
            let result = follow(raptor, &plan, known, &mut done).await;
            save_followed(
                pool,
                project_id,
                &done,
                result.as_ref().err().map(String::as_str),
            )
            .await?;
            match result {
                Ok(()) => summary.updated += 1,
                Err(error) => {
                    lambda_http::tracing::warn!(project_id, %error, "Raptor job update failed");
                }
            }
        }
    }
    Ok(summary)
}

/// Makes a new project's Raptor jobs.
async fn new_project(
    pool: &MySqlPool,
    config: &Config,
    login: &Login,
    project_id: i32,
    raptor: &Raptor,
    known: &mut Vec<ListJob>,
    summary: &mut RunSummary,
) -> sqlx::Result<()> {
    let Some(mut done) = claim(pool, project_id, login.company_id).await? else {
        return Ok(());
    };
    let result = match load_plan(pool, project_id, login.company_id).await? {
        Ok(plan) => {
            let plan = config.named(login.company_id, plan);
            send(raptor, &plan, known, &mut done).await
        }
        Err(why) => Err(why),
    };
    match result {
        Ok(()) => {
            finish(pool, project_id, &done, None).await?;
            summary.done += 1;
        }
        Err(error) => {
            lambda_http::tracing::warn!(project_id, %error, "Raptor job failed");
            finish(pool, project_id, &done, Some(&error)).await?;
            summary.failed += 1;
        }
    }
    Ok(())
}

/// Deletes the Raptor jobs of the company's unsold projects (`removing`) and
/// forgets them, so selling again makes them anew; jobs the office made are
/// left. `false` when Raptor does not let the company in.
async fn remove_unsold(
    pool: &MySqlPool,
    config: &Config,
    login: &Login,
    session: &mut Option<Session>,
    summary: &mut RunSummary,
) -> sqlx::Result<bool> {
    // A first try waits 3 minutes when a run was still sending the project
    // (`unsold` leaves `updated_at` at now); later tries come 10 minutes apart.
    let rows: Vec<(i32, Option<String>)> = sqlx::query_as(
        "SELECT project_id, CAST(jobs AS CHAR) FROM project_raptor_jobs
          WHERE company_id = ? AND status = 'removing' AND attempts < ?
            AND updated_at <= NOW() - INTERVAL IF(attempts = 0, 3, 10) MINUTE
          ORDER BY updated_at
          LIMIT 20",
    )
    .bind(login.company_id)
    .bind(MAX_REMOVALS)
    .fetch_all(pool)
    .await?;
    for (project_id, jobs) in rows {
        let jobs: Vec<JobRecord> = jobs
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default();
        let mut result = Ok(());
        let ours: Vec<&JobRecord> = jobs.iter().filter(|j| !j.existing).collect();
        if !ours.is_empty() {
            if session.is_none() {
                match open(config, login).await {
                    Ok(open) => *session = Some(open),
                    Err(error) => {
                        lambda_http::tracing::warn!(company_id = login.company_id, %error, "Raptor login failed");
                        return Ok(false);
                    }
                }
            }
            let Some(Session { raptor, known }) = session.as_mut() else {
                return Ok(false);
            };
            for job in ours {
                result = raptor.delete_job(&job.id).await;
                if result.is_err() {
                    break;
                }
                if let Some(listed) = known.iter_mut().find(|k| k.id == job.id) {
                    listed.status = Some("Deleted".into());
                }
            }
        }
        match result {
            Ok(()) => {
                // Sold again meanwhile: the row stays, without the deleted jobs.
                let gone = sqlx::query(
                    "DELETE FROM project_raptor_jobs WHERE project_id = ? AND status = 'removing'",
                )
                .bind(project_id)
                .execute(pool)
                .await?;
                if gone.rows_affected() == 0 {
                    sqlx::query("UPDATE project_raptor_jobs SET jobs = NULL WHERE project_id = ?")
                        .bind(project_id)
                        .execute(pool)
                        .await?;
                }
                summary.removed += 1;
            }
            Err(error) => {
                lambda_http::tracing::warn!(project_id, %error, "Raptor job not deleted");
                let error: String = error.chars().take(1000).collect();
                sqlx::query(
                    "UPDATE project_raptor_jobs
                        SET attempts = attempts + 1, error = ?, updated_at = NOW()
                      WHERE project_id = ? AND status = 'removing'",
                )
                .bind(error)
                .bind(project_id)
                .execute(pool)
                .await?;
            }
        }
    }
    Ok(true)
}

/// Mark as Sold (a sale company's): queues the project of won deal `deal_id`.
///
/// It gets its Raptor jobs whenever it was created. One already sent
/// stays as it is (its jobs follow the CRM); one that failed or is being
/// deleted starts over. Nothing before the Raptor login is saved. Logged,
/// never returned: the sale is already saved.
pub async fn sold(pool: &MySqlPool, config: &Config, company_id: i32, deal_id: i64) {
    if !config.sales(company_id) {
        return;
    }
    let queued = sqlx::query(
        "INSERT INTO project_raptor_jobs (project_id, company_id, status, attempts)
         SELECT p.id, p.company_id, 'waiting', 0
           FROM projects p JOIN company c ON c.id = p.company_id
          WHERE p.company_id = ? AND p.source_deal_id = ?
            AND p.cancelled_at IS NULL AND p.archived_at IS NULL
            AND c.raptor_since IS NOT NULL
         ON DUPLICATE KEY UPDATE
            attempts = IF(project_raptor_jobs.status IN ('failed', 'removing'),
                          0, project_raptor_jobs.attempts),
            error = IF(project_raptor_jobs.status IN ('failed', 'removing'),
                       NULL, project_raptor_jobs.error),
            status = IF(project_raptor_jobs.status IN ('failed', 'removing'),
                        'waiting', project_raptor_jobs.status)",
    )
    .bind(company_id)
    .bind(deal_id)
    .execute(pool)
    .await;
    if let Err(error) = queued {
        lambda_http::tracing::error!(?error, deal_id, "could not queue the sale's Raptor jobs");
    }
}

/// Undo sale (a sale company's): the Raptor jobs of the projects of
/// `deal_id` are deleted by the next run; a project still waiting is just
/// forgotten. Logged, never returned: the sale is already undone.
pub async fn unsold(pool: &MySqlPool, config: &Config, company_id: i32, deal_id: Option<i64>) {
    let Some(deal_id) = deal_id.filter(|_| config.sales(company_id)) else {
        return;
    };
    // A run may be sending the project right now (`working`): its jobs are
    // saved when it ends, so the deleting waits (`updated_at` now).
    let marked = sqlx::query(
        "UPDATE project_raptor_jobs
            SET attempts = 0, error = NULL,
                updated_at = IF(status = 'working', NOW(), NOW() - INTERVAL 1 HOUR),
                status = 'removing'
          WHERE status <> 'removing'
            AND project_id IN (SELECT id FROM projects
                                WHERE company_id = ? AND source_deal_id = ?)",
    )
    .bind(company_id)
    .bind(deal_id)
    .execute(pool)
    .await;
    if let Err(error) = marked {
        lambda_http::tracing::error!(
            ?error,
            deal_id,
            "could not mark the Raptor jobs for deletion"
        );
    }
}
