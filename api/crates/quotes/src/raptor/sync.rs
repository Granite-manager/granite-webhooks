//! Sends each new project of a company with a Raptor login to Raptor.
//!
//! Only projects created since the login was saved (`company.raptor_since`),
//! once each. A project is claimed in `project_raptor_jobs`, its Raptor jobs
//! are made and filled, and the row says `done` with the jobs, or `failed`
//! with the reason; a failed project is tried again 10 minutes later, three
//! times in all, and carries on from the jobs it already made.

use super::client::{API, ListJob, Raptor, next_number};
use super::plan::{self, Plan, Source};
use crate::quote_pricing::doc::QuoteDoc;
use common::telephony::ringcentral_auth::{decrypt, env_key};
use serde::{Deserialize, Serialize};
use sqlx::{MySqlPool, Row};
use std::time::{Duration, Instant};

const MAX_ATTEMPTS: i32 = 3;

/// Where Raptor is and the key that opens the saved passwords.
pub struct Config {
    pub api_url: String,
    pub key: Option<[u8; 32]>,
}

impl Config {
    /// Raptor's API (or `RAPTOR_API_URL`) and `AES_KEY`.
    pub fn from_env() -> Self {
        Self {
            api_url: std::env::var("RAPTOR_API_URL").unwrap_or_else(|_| API.to_string()),
            key: env_key(),
        }
    }
}

/// What a run did.
#[derive(Debug, Default, Serialize)]
pub struct RunSummary {
    /// Projects whose Raptor jobs are all made.
    pub done: u32,
    /// Projects that failed this time.
    pub failed: u32,
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
            if status == "done" || attempts >= MAX_ATTEMPTS || recent {
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
    sqlx::query(
        "UPDATE project_raptor_jobs SET status = ?, jobs = ?, error = ? WHERE project_id = ?",
    )
    .bind(if error.is_some() { "failed" } else { "done" })
    .bind(serde_json::to_string(jobs).unwrap_or_default())
    .bind(error)
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
        let index = if let Some(index) = done.iter().position(|r| r.name == job.name) {
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
            let reference = format!("#{}", next_number(known));
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
            raptor.add_note(&record.id, &job.notes).await?;
            record.noted = true;
        }
    }
    Ok(())
}

/// Sends the due projects of every company with a Raptor login, until
/// `budget` is spent.
pub async fn run(pool: &MySqlPool, budget: Duration, config: &Config) -> sqlx::Result<RunSummary> {
    let started = Instant::now();
    let mut summary = RunSummary::default();
    for login in logins(pool, config.key.as_ref()).await? {
        let due = due_projects(pool, login.company_id).await?;
        if due.is_empty() {
            continue;
        }
        let http = common::http::client();
        let raptor = match Raptor::login(http, &config.api_url, &login.email, &login.password).await
        {
            Ok(raptor) => raptor,
            Err(error) => {
                lambda_http::tracing::warn!(company_id = login.company_id, %error, "Raptor login failed");
                continue;
            }
        };
        let mut known = match raptor.jobs().await {
            Ok(jobs) => jobs,
            Err(error) => {
                lambda_http::tracing::warn!(company_id = login.company_id, %error, "Raptor job list failed");
                continue;
            }
        };
        for project_id in due {
            if started.elapsed() > budget {
                return Ok(summary);
            }
            let Some(mut done) = claim(pool, project_id, login.company_id).await? else {
                continue;
            };
            let result = match load_plan(pool, project_id, login.company_id).await? {
                Ok(plan) => send(&raptor, &plan, &mut known, &mut done).await,
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
        }
    }
    Ok(summary)
}
