//! Pay for templates and installs (Admin > Commissions): what a template
//! technician or an installer earns per sq ft of every job they finished,
//! plus extras an admin adds by hand (a repair, a tear-out).
//!
//! - `GET    /v1/job-pay-rules`, `POST`, `PUT|DELETE /v1/job-pay-rules/{id}` → dollars per sq ft, and the extras to pick from
//! - `GET    /v1/job-pay` → every finished template and install with what it pays
//! - `PUT    /v1/job-pay/{id}` → an admin's own sq ft, amount, extras and payroll date
//! - `POST   /v1/job-pay/{id}/payroll` → add a job to the salary
//! - `POST   /v1/job-pay/payouts` → mark jobs in the salary paid out
//!
//! The rows are made by the commissions sync (`POST /v1/commissions/sync`).

use super::commissions::{Payout, load_settings};
use crate::auth::AdminUser;
use crate::commissions::{money, next_week_payroll, payroll_date_for, stone_sq_ft};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::quote_pricing::pricing::price_quote;
use crate::quote_pricing::{load_company_quote, price_list_for_quote};
use crate::routes::created::CreatedId;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;
use std::collections::{HashMap, HashSet};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_rules, create_rule))
        .routes(routes!(update_rule, delete_rule))
        .routes(routes!(list_job_pay))
        .routes(routes!(update_job_pay))
        .routes(routes!(add_to_payroll))
        .routes(routes!(pay_out))
}

/// The Installer and Template technician positions: a rule for everyone
/// pays the holders of the work's position.
const INSTALLER: i32 = 6;
const TEMPLATE_TECHNICIAN: i32 = 11;

fn position_of(work: &str) -> i32 {
    if work == "template" {
        TEMPLATE_TECHNICIAN
    } else {
        INSTALLER
    }
}

/// A pay rule for templates or installs.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobPayRule {
    /// Rule id.
    pub id: i64,
    /// Name ("Install", "Tear-out").
    pub name: String,
    /// `template` or `install`.
    pub work: String,
    /// `sqft` (dollars per sq ft of every finished job, by itself) or `extra`
    /// (a fixed amount an admin adds to a job by hand).
    pub kind: String,
    /// Dollars per sq ft, or the extra's dollars.
    pub amount: f64,
    /// `sqft` rules: the one person it pays; null: everyone with the work's
    /// position (Template technician, Installer).
    pub user_id: Option<i32>,
    /// That person's name.
    pub person: Option<String>,
    /// Off rules pay nothing.
    pub active: bool,
}

#[derive(sqlx::FromRow)]
struct RuleRow {
    id: i32,
    name: String,
    work: String,
    kind: String,
    amount: f64,
    user_id: Option<i32>,
    person: Option<String>,
    active: i8,
}

async fn load_rules(pool: &MySqlPool, company_id: i32) -> ApiResult<Vec<JobPayRule>> {
    let rows: Vec<RuleRow> = sqlx::query_as(
        "SELECT r.id, r.name, r.work, r.kind, CAST(r.amount AS DOUBLE) AS amount, r.user_id,
                u.name AS person, r.active
           FROM job_pay_rules r
           LEFT JOIN users u ON u.id = r.user_id
          WHERE r.company_id = ? AND r.deleted_at IS NULL
          ORDER BY r.work DESC, r.kind DESC, r.user_id IS NOT NULL, r.id",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| JobPayRule {
            id: i64::from(r.id),
            name: r.name,
            work: r.work,
            kind: r.kind,
            amount: r.amount,
            user_id: r.user_id,
            person: r.person,
            active: r.active == 1,
        })
        .collect())
}

/// List the template and install pay rules
///
/// Templates first, then installs; the per sq ft rules before the extras.
#[crate::openapi::path(
    get,
    path = "/v1/job-pay-rules",
    operation_id = "list_job_pay_rules",
    tag = "commissions",
    responses(
        (status = 200, description = "The rules", body = [JobPayRule]),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_rules(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<Vec<JobPayRule>>> {
    Ok(Json(load_rules(&state.pool, user.company_id).await?))
}

/// A pay rule as an admin builds it.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobPayRuleInput {
    /// Name (1 to 120 characters).
    pub name: String,
    /// `template` or `install`.
    pub work: String,
    /// `sqft` or `extra`.
    pub kind: String,
    /// Dollars per sq ft, or the extra's dollars; not negative.
    pub amount: f64,
    /// `sqft` rules: one person; null: everyone with the work's position.
    #[serde(default)]
    pub user_id: Option<i32>,
    /// Off rules pay nothing.
    #[serde(default = "yes")]
    pub active: bool,
}

const fn yes() -> bool {
    true
}

async fn save_rule(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
    id: Option<i32>,
    input: JobPayRuleInput,
) -> ApiResult<i64> {
    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::BadRequest("Name is required".into()));
    }
    if name.chars().count() > 120
        || !matches!(input.work.as_str(), "template" | "install")
        || !matches!(input.kind.as_str(), "sqft" | "extra")
    {
        return Err(ApiError::BadRequest("Check the rule.".into()));
    }
    if !input.amount.is_finite() || input.amount < 0.0 {
        return Err(ApiError::BadRequest("Amount cannot be negative".into()));
    }
    // Only a per sq ft rule is one person's: an extra is picked by hand.
    let person = input.user_id.filter(|_| input.kind == "sqft");
    if let Some(person) = person {
        let found: Option<i32> = sqlx::query_scalar(
            "SELECT id FROM users WHERE id = ? AND company_id = ? AND is_deleted = 0",
        )
        .bind(person)
        .bind(company_id)
        .fetch_optional(pool)
        .await?;
        if found.is_none() {
            return Err(ApiError::BadRequest("Pick a person of the company".into()));
        }
    }
    let amount = money(input.amount);
    if let Some(id) = id {
        let updated = sqlx::query(
            "UPDATE job_pay_rules
                SET name = ?, work = ?, kind = ?, amount = ?, user_id = ?, active = ?
              WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
        )
        .bind(&name)
        .bind(&input.work)
        .bind(&input.kind)
        .bind(amount)
        .bind(person)
        .bind(input.active)
        .bind(id)
        .bind(company_id)
        .execute(pool)
        .await?
        .rows_affected();
        if updated == 0 {
            let found: Option<i32> = sqlx::query_scalar(
                "SELECT id FROM job_pay_rules WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
            )
            .bind(id)
            .bind(company_id)
            .fetch_optional(pool)
            .await?;
            if found.is_none() {
                return Err(ApiError::NotFound);
            }
        }
        return Ok(i64::from(id));
    }
    let id = sqlx::query(
        "INSERT INTO job_pay_rules (company_id, name, work, kind, amount, user_id, active, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(company_id)
    .bind(&name)
    .bind(&input.work)
    .bind(&input.kind)
    .bind(amount)
    .bind(person)
    .bind(input.active)
    .bind(user_id)
    .execute(pool)
    .await?
    .last_insert_id();
    // Jobs count from the day the company's first rule was made.
    sqlx::query(
        "INSERT INTO commission_settings (company_id, job_pay_from) VALUES (?, CURDATE())
         ON DUPLICATE KEY UPDATE job_pay_from = COALESCE(job_pay_from, CURDATE())",
    )
    .bind(company_id)
    .execute(pool)
    .await?;
    Ok(i64::try_from(id).unwrap_or_default())
}

/// Add a template or install pay rule
///
/// It applies from the next sync. The company's first rule starts the
/// count: jobs finished from that day on are paid (`jobPayFrom` in the
/// commission settings).
#[crate::openapi::path(
    post,
    path = "/v1/job-pay-rules",
    operation_id = "create_job_pay_rule",
    tag = "commissions",
    request_body = JobPayRuleInput,
    responses(
        (status = 201, description = "Added", body = CreatedId),
        (status = 400, description = "Check the rule", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_rule(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<JobPayRuleInput>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    let id = save_rule(&state.pool, user.company_id, user.id, None, input).await?;
    Ok((StatusCode::CREATED, Json(CreatedId { id })))
}

/// Save a template or install pay rule
///
/// Replaces the whole rule; it applies from the next sync.
#[crate::openapi::path(
    put,
    path = "/v1/job-pay-rules/{id}",
    operation_id = "update_job_pay_rule",
    tag = "commissions",
    params(("id" = i32, Path, description = "Rule id")),
    request_body = JobPayRuleInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Check the rule", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such rule", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_rule(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<JobPayRuleInput>,
) -> ApiResult<StatusCode> {
    save_rule(&state.pool, user.company_id, user.id, Some(id), input).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a template or install pay rule
///
/// Jobs already worked out keep what they pay.
#[crate::openapi::path(
    delete,
    path = "/v1/job-pay-rules/{id}",
    operation_id = "delete_job_pay_rule",
    tag = "commissions",
    params(("id" = i32, Path, description = "Rule id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_rule(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    sqlx::query("UPDATE job_pay_rules SET deleted_at = NOW() WHERE id = ? AND company_id = ?")
        .bind(id)
        .bind(user.company_id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// An extra on a job: a repair, a tear-out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct JobPayExtra {
    /// What it is for (1 to 120 characters).
    pub label: String,
    /// Dollars; not negative.
    pub amount: f64,
}

/// A finished template or install and what it pays one person.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobPayRow {
    /// Row id.
    pub id: i32,
    /// The schedule event.
    pub event_id: i32,
    /// `template` or `install`.
    pub work: String,
    /// Who did it.
    pub user_id: i32,
    /// Their name.
    pub person: String,
    /// The customer, or the event's title when it has none.
    pub job: String,
    /// The day it was finished (YYYY-MM-DD).
    pub done_date: String,
    /// The sq ft of stone on the event's quote; null: no quote, or no stone on it.
    pub quote_sq_ft: Option<f64>,
    /// The sq ft paid: an admin's own, else the quote's; null: not known yet.
    pub sq_ft: Option<f64>,
    /// Dollars per sq ft.
    pub rate: f64,
    /// The job's pay before extras: an admin's own, else sq ft × rate.
    pub amount: f64,
    /// An admin set the sq ft or the amount.
    pub adjusted: bool,
    /// Extras added by hand.
    pub extras: Vec<JobPayExtra>,
    /// `amount` plus the extras.
    pub total: f64,
    /// The payroll it is paid with (YYYY-MM-DD); null: not in the salary yet.
    pub payroll_date: Option<String>,
    /// Paid out to the person.
    pub paid_out: bool,
}

#[derive(sqlx::FromRow)]
struct PayRow {
    id: i32,
    event_id: i32,
    work: String,
    user_id: i32,
    person: Option<String>,
    job: Option<String>,
    done_date: String,
    quote_sq_ft: Option<f64>,
    sq_ft_override: Option<f64>,
    rate: f64,
    amount_override: Option<f64>,
    payroll_date: Option<String>,
    paid_out: i64,
}

/// The finished jobs and what they pay, newest first: one person's with
/// `user_id`.
pub async fn load_job_pay(
    pool: &MySqlPool,
    company_id: i32,
    user_id: Option<i32>,
) -> ApiResult<Vec<JobPayRow>> {
    let rows: Vec<PayRow> = sqlx::query_as(
        "SELECT jp.id, jp.event_id, jp.work, jp.user_id, u.name AS person,
                COALESCE(NULLIF(TRIM(c.name), ''), e.title) AS job,
                DATE_FORMAT(jp.done_date, '%Y-%m-%d') AS done_date,
                CAST(jp.sq_ft AS DOUBLE) AS quote_sq_ft,
                CAST(jp.sq_ft_override AS DOUBLE) AS sq_ft_override,
                CAST(jp.rate AS DOUBLE) AS rate,
                CAST(jp.amount_override AS DOUBLE) AS amount_override,
                DATE_FORMAT(jp.payroll_date, '%Y-%m-%d') AS payroll_date,
                CAST(jp.paid_out_at IS NOT NULL AS SIGNED) AS paid_out
           FROM job_pay jp
           JOIN events e ON e.id = jp.event_id
           LEFT JOIN customers c ON c.id = e.customer_id
           LEFT JOIN users u ON u.id = jp.user_id
          WHERE jp.company_id = ? AND (? IS NULL OR jp.user_id = ?)
          ORDER BY jp.done_date DESC, jp.id DESC",
    )
    .bind(company_id)
    .bind(user_id)
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    let extras: Vec<(i32, String, f64)> = sqlx::query_as(
        "SELECT job_pay_id, label, CAST(amount AS DOUBLE) FROM job_pay_extras
          WHERE company_id = ? ORDER BY id",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    let mut by_row: HashMap<i32, Vec<JobPayExtra>> = HashMap::new();
    for (row, label, amount) in extras {
        by_row
            .entry(row)
            .or_default()
            .push(JobPayExtra { label, amount });
    }
    Ok(rows
        .into_iter()
        .map(|r| {
            let sq_ft = r.sq_ft_override.or(r.quote_sq_ft);
            let amount = r
                .amount_override
                .unwrap_or_else(|| money(sq_ft.unwrap_or(0.0) * r.rate));
            let extras = by_row.remove(&r.id).unwrap_or_default();
            let total = money(extras.iter().fold(amount, |sum, extra| sum + extra.amount));
            JobPayRow {
                id: r.id,
                event_id: r.event_id,
                work: r.work,
                user_id: r.user_id,
                person: r.person.unwrap_or_else(|| "Unassigned".into()),
                job: r.job.unwrap_or_default(),
                done_date: r.done_date,
                quote_sq_ft: r.quote_sq_ft,
                sq_ft,
                rate: r.rate,
                amount,
                adjusted: r.sq_ft_override.is_some() || r.amount_override.is_some(),
                extras,
                total,
                payroll_date: r.payroll_date,
                paid_out: r.paid_out == 1,
            }
        })
        .collect())
}

/// List template and install pay
///
/// Every finished template and install since the company's first pay rule,
/// per person who did it, newest first, with what it pays.
#[crate::openapi::path(
    get,
    path = "/v1/job-pay",
    operation_id = "list_job_pay",
    tag = "commissions",
    responses(
        (status = 200, description = "The jobs", body = [JobPayRow]),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_job_pay(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<Vec<JobPayRow>>> {
    Ok(Json(
        load_job_pay(&state.pool, user.company_id, None).await?,
    ))
}

/// The sq ft of stone on a quote (the option the customer took); none when
/// the quote is gone or has no stone.
async fn quote_sq_ft(state: &AppState, company_id: i32, quote_id: i32) -> ApiResult<Option<f64>> {
    let Some(quote) = load_company_quote(&state.pool, quote_id, company_id).await? else {
        return Ok(None);
    };
    let doc = quote.doc();
    let price_list = price_list_for_quote(&state.pool, &quote).await?;
    let prices = price_quote(&doc, &price_list, quote.options());
    let accepted = usize::try_from(quote.customer_accepted_option_id.unwrap_or(0).max(0))
        .unwrap_or_default()
        .min(prices.len().saturating_sub(1));
    Ok(prices
        .get(accepted)
        .map(stone_sq_ft)
        .filter(|sq_ft| *sq_ft > 0.0)
        .map(money))
}

#[derive(sqlx::FromRow)]
struct DoneEvent {
    id: i32,
    slug: String,
    quote_id: Option<i32>,
    assigned_user_id: Option<i32>,
    done_date: String,
}

/// The finished templates and installs: marked completed by the crew, or
/// their time has gone by. Repeating events are not jobs. (A macro: the
/// queries below must be string literals.)
macro_rules! done_events {
    () => {
        "FROM events e
           JOIN calendars cal ON cal.id = e.calendar_id
          WHERE cal.company_id = ? AND cal.deleted_at IS NULL
            AND cal.slug IN ('templates', 'installations')
            AND e.deleted_date IS NULL AND COALESCE(e.unscheduled, 0) = 0 AND e.rrule IS NULL
            AND (e.completed_at IS NOT NULL OR e.crew_status = 'completed'
                 OR COALESCE(e.end_date, e.start_date) < NOW())
            AND DATE(COALESCE(e.completed_at, e.start_date)) >= ?"
    };
}

/// Works out what every finished template and install pays, for the
/// commissions sync: a row per job and person who did it (the event's
/// assignee and its people) with a per sq ft rule, their own or their
/// position's. Jobs already in the salary are left alone; ones no longer
/// finished, or whose person left the job, go (unless an admin set their
/// amounts or added extras). Returns the jobs worked out.
#[allow(clippy::too_many_lines)]
pub async fn sync_job_pay(state: &AppState, company_id: i32) -> ApiResult<usize> {
    let pool = &state.pool;
    let from: Option<String> = sqlx::query_scalar::<_, Option<String>>(
        "SELECT DATE_FORMAT(job_pay_from, '%Y-%m-%d') FROM commission_settings WHERE company_id = ?",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await?
    .flatten();
    // No rule was ever made: nothing is paid per job.
    let Some(from) = from else {
        return Ok(0);
    };
    let rates: Vec<(String, Option<i32>, f64)> = sqlx::query_as(
        "SELECT work, user_id, CAST(amount AS DOUBLE) FROM job_pay_rules
          WHERE company_id = ? AND deleted_at IS NULL AND active = 1 AND kind = 'sqft'",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    // Rules add up; a person's own rules replace their position's.
    let mut own: HashMap<(String, i32), f64> = HashMap::new();
    let mut everyone: HashMap<String, f64> = HashMap::new();
    for (work, person, amount) in rates {
        match person {
            Some(person) => *own.entry((work, person)).or_default() += amount,
            None => *everyone.entry(work).or_default() += amount,
        }
    }
    let holders: HashSet<(i32, i32)> = sqlx::query_as::<_, (i32, i32)>(
        "SELECT up.position_id, up.user_id FROM users_positions up
           JOIN users u ON u.id = up.user_id AND u.is_deleted = 0
          WHERE up.company_id = ? AND up.position_id IN (?, ?)",
    )
    .bind(company_id)
    .bind(INSTALLER)
    .bind(TEMPLATE_TECHNICIAN)
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();
    let events: Vec<DoneEvent> = sqlx::query_as(concat!(
        "SELECT e.id, cal.slug, e.quote_id, e.assigned_user_id,
                DATE_FORMAT(COALESCE(e.completed_at, e.start_date), '%Y-%m-%d') AS done_date ",
        done_events!()
    ))
    .bind(company_id)
    .bind(&from)
    .fetch_all(pool)
    .await?;
    let people: Vec<(i32, i32)> = sqlx::query_as(concat!(
        "SELECT ep.event_id, ep.user_id FROM event_people ep
          WHERE ep.event_id IN (SELECT e.id ",
        done_events!(),
        ")"
    ))
    .bind(company_id)
    .bind(&from)
    .fetch_all(pool)
    .await?;
    let mut crew: HashMap<i32, Vec<i32>> = HashMap::new();
    for (event, person) in people {
        crew.entry(event).or_default().push(person);
    }

    let mut sq_ft_of: HashMap<i32, Option<f64>> = HashMap::new();
    let mut wanted: HashSet<(i32, i32)> = HashSet::new();
    let mut synced = 0;
    for event in events {
        let work = if event.slug == "templates" {
            "template"
        } else {
            "install"
        };
        let mut who: Vec<i32> = event.assigned_user_id.into_iter().collect();
        for person in crew.remove(&event.id).unwrap_or_default() {
            if !who.contains(&person) {
                who.push(person);
            }
        }
        for person in who {
            let rate = own.get(&(work.to_string(), person)).copied().or_else(|| {
                holders
                    .contains(&(position_of(work), person))
                    .then(|| everyone.get(work).copied())
                    .flatten()
            });
            let Some(rate) = rate else {
                continue;
            };
            let sq_ft = match event.quote_id {
                None => None,
                Some(quote) => match sq_ft_of.get(&quote) {
                    Some(known) => *known,
                    None => {
                        let found = quote_sq_ft(state, company_id, quote).await?;
                        sq_ft_of.insert(quote, found);
                        found
                    }
                },
            };
            sqlx::query(
                "INSERT INTO job_pay (company_id, event_id, user_id, work, sq_ft, rate, done_date)
                 VALUES (?, ?, ?, ?, ?, ?, ?)
                 ON DUPLICATE KEY UPDATE
                   work = IF(payroll_date IS NULL, VALUES(work), work),
                   sq_ft = IF(payroll_date IS NULL, VALUES(sq_ft), sq_ft),
                   rate = IF(payroll_date IS NULL, VALUES(rate), rate),
                   done_date = IF(payroll_date IS NULL, VALUES(done_date), done_date),
                   synced_at = NOW()",
            )
            .bind(company_id)
            .bind(event.id)
            .bind(person)
            .bind(work)
            .bind(sq_ft)
            .bind(money(rate))
            .bind(&event.done_date)
            .execute(pool)
            .await?;
            wanted.insert((event.id, person));
            synced += 1;
        }
    }

    // Rows nobody touched whose job is no longer finished, or no longer theirs.
    let untouched: Vec<(i32, i32, i32)> = sqlx::query_as(
        "SELECT jp.id, jp.event_id, jp.user_id FROM job_pay jp
          WHERE jp.company_id = ? AND jp.payroll_date IS NULL
            AND jp.sq_ft_override IS NULL AND jp.amount_override IS NULL
            AND NOT EXISTS (SELECT 1 FROM job_pay_extras x WHERE x.job_pay_id = jp.id)",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    for (id, event, person) in untouched {
        if !wanted.contains(&(event, person)) {
            sqlx::query("DELETE FROM job_pay WHERE id = ? AND company_id = ?")
                .bind(id)
                .bind(company_id)
                .execute(pool)
                .await?;
        }
    }
    Ok(synced)
}

/// An admin's own numbers for a job.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobPayEdit {
    /// The sq ft paid; null: the quote's.
    #[serde(default)]
    pub sq_ft: Option<f64>,
    /// The job's pay before extras; null: sq ft × rate.
    #[serde(default)]
    pub amount: Option<f64>,
    /// The payroll it is paid with (YYYY-MM-DD); null: not in the salary.
    #[serde(default)]
    pub payroll_date: Option<String>,
    /// The extras: replaces the job's list.
    #[serde(default)]
    pub extras: Vec<JobPayExtra>,
}

fn ymd(text: &str) -> Option<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(text.get(..10)?, "%Y-%m-%d").ok()
}

/// Set a job's sq ft, amount, extras and payroll date
///
/// Null `sqFt` and `amount` go back to the quote's sq ft and to sq ft × rate.
/// `extras` replaces the job's extras (a repair, a tear-out). Taking the
/// payroll date away also takes "paid out" away.
#[crate::openapi::path(
    put,
    path = "/v1/job-pay/{id}",
    operation_id = "update_job_pay",
    tag = "commissions",
    params(("id" = i32, Path, description = "Job pay row id")),
    request_body = JobPayEdit,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "A negative number, an extra with no name or a bad date", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such job", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_job_pay(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<JobPayEdit>,
) -> ApiResult<StatusCode> {
    let number = |value: Option<f64>| -> ApiResult<Option<f64>> {
        match value {
            Some(v) if !v.is_finite() || v < 0.0 => {
                Err(ApiError::BadRequest("Amounts cannot be negative.".into()))
            }
            other => Ok(other.map(money)),
        }
    };
    let (sq_ft, amount) = (number(input.sq_ft)?, number(input.amount)?);
    let mut extras = Vec::with_capacity(input.extras.len());
    for extra in input.extras {
        let label = extra.label.trim().to_string();
        if label.is_empty() || label.chars().count() > 120 {
            return Err(ApiError::BadRequest("Name every extra.".into()));
        }
        let Some(amount) = number(Some(extra.amount))? else {
            continue;
        };
        extras.push((label, amount));
    }
    let payroll = match input.payroll_date.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(text) => Some(
            ymd(text)
                .filter(|_| text.len() == 10)
                .ok_or_else(|| ApiError::BadRequest("Pick the payroll date.".into()))?
                .format("%Y-%m-%d")
                .to_string(),
        ),
    };
    let found: Option<i32> =
        sqlx::query_scalar("SELECT id FROM job_pay WHERE id = ? AND company_id = ?")
            .bind(id)
            .bind(user.company_id)
            .fetch_optional(&state.pool)
            .await?;
    if found.is_none() {
        return Err(ApiError::NotFound);
    }
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "UPDATE job_pay
            SET sq_ft_override = ?, amount_override = ?,
                paid_out_at = IF(? IS NULL, NULL, paid_out_at), payroll_date = ?
          WHERE id = ? AND company_id = ?",
    )
    .bind(sq_ft)
    .bind(amount)
    .bind(payroll.as_deref())
    .bind(payroll.as_deref())
    .bind(id)
    .bind(user.company_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM job_pay_extras WHERE job_pay_id = ? AND company_id = ?")
        .bind(id)
        .bind(user.company_id)
        .execute(&mut *tx)
        .await?;
    for (label, amount) in extras {
        sqlx::query(
            "INSERT INTO job_pay_extras (company_id, job_pay_id, label, amount) VALUES (?, ?, ?, ?)",
        )
        .bind(user.company_id)
        .bind(id)
        .bind(label)
        .bind(amount)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Where a job went in the salary.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobPayAdded {
    /// The payroll it is paid with (YYYY-MM-DD).
    pub payroll_date: String,
}

/// The payroll a finished job is paid with: the payroll day of the week it
/// was finished in (`same_week`: that day or the first one after it), or of
/// the week after; the next payroll from `today` when that one has gone by.
pub fn job_payroll(
    done: chrono::NaiveDate,
    today: chrono::NaiveDate,
    weekday: u32,
    same_week: bool,
) -> chrono::NaiveDate {
    if !same_week {
        return next_week_payroll(done, today, weekday);
    }
    let on_or_after = |day: chrono::NaiveDate| {
        payroll_date_for(&day.format("%Y-%m-%d").to_string(), weekday)
            .as_deref()
            .and_then(ymd)
            .unwrap_or(day)
    };
    let payday = on_or_after(done);
    if payday >= today {
        payday
    } else {
        on_or_after(today)
    }
}

/// Add a job to the salary
///
/// A finished template or install goes into the payroll the company's
/// commission settings say: the payroll day of the week after the one it
/// was finished in, or (`jobPaySameWeek`) the first payroll day on or after
/// the day it was finished. When that payroll has gone by, the next one.
#[crate::openapi::path(
    post,
    path = "/v1/job-pay/{id}/payroll",
    operation_id = "add_job_pay_to_payroll",
    tag = "commissions",
    params(("id" = i32, Path, description = "Job pay row id")),
    responses(
        (status = 200, description = "Added", body = JobPayAdded),
        (status = 400, description = "Already in the salary", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such job", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn add_to_payroll(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<JobPayAdded>> {
    let row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT DATE_FORMAT(done_date, '%Y-%m-%d'), DATE_FORMAT(payroll_date, '%Y-%m-%d')
           FROM job_pay WHERE id = ? AND company_id = ?",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?;
    let (done, payroll) = row.ok_or(ApiError::NotFound)?;
    if let Some(date) = payroll {
        return Err(ApiError::BadRequest(format!(
            "It is already in the salary for the payroll of {date}."
        )));
    }
    let today = chrono::Utc::now().date_naive();
    let settings = load_settings(&state.pool, user.company_id).await?;
    let payroll = job_payroll(
        ymd(&done).unwrap_or(today),
        today,
        settings.payroll_weekday,
        settings.job_pay_same_week.unwrap_or(false),
    )
    .format("%Y-%m-%d")
    .to_string();
    sqlx::query(
        "UPDATE job_pay SET payroll_date = ?
          WHERE id = ? AND company_id = ? AND payroll_date IS NULL",
    )
    .bind(&payroll)
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?;
    Ok(Json(JobPayAdded {
        payroll_date: payroll,
    }))
}

/// Mark jobs paid to the person
///
/// Marks them paid out today; one not in the salary yet goes into today's
/// paycheck. With `paid: false` they are not paid out any more (and keep
/// their payroll date).
#[crate::openapi::path(
    post,
    path = "/v1/job-pay/payouts",
    operation_id = "pay_out_job_pay",
    tag = "commissions",
    request_body = Payout,
    responses(
        (status = 204, description = "Marked"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn pay_out(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<Payout>,
) -> ApiResult<StatusCode> {
    if input.ids.is_empty() {
        return Ok(StatusCode::NO_CONTENT);
    }
    let today = chrono::Utc::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string();
    let mut sql = sqlx::QueryBuilder::new("UPDATE job_pay SET ");
    if input.paid {
        sql.push("paid_out_at = NOW(), payroll_date = COALESCE(payroll_date, ")
            .push_bind(today)
            .push(") WHERE paid_out_at IS NULL");
    } else {
        sql.push("paid_out_at = NULL WHERE paid_out_at IS NOT NULL");
    }
    sql.push(" AND company_id = ")
        .push_bind(user.company_id)
        .push(" AND id IN (");
    let mut ids = sql.separated(", ");
    for id in input.ids {
        ids.push_bind(id);
    }
    sql.push(")");
    sql.build().execute(&state.pool).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(text: &str) -> chrono::NaiveDate {
        ymd(text).unwrap()
    }

    #[test]
    fn a_job_is_paid_the_same_week_or_the_week_after() {
        // Finished Tuesday 11/10/2026; payroll on Fridays (5).
        let done = d("2026-11-10");
        // The week after: Friday 11/20. The same week: Friday 11/13.
        assert_eq!(job_payroll(done, done, 5, false), d("2026-11-20"));
        assert_eq!(job_payroll(done, done, 5, true), d("2026-11-13"));
        // Finished on a payday: that same day.
        assert_eq!(
            job_payroll(d("2026-11-13"), d("2026-11-13"), 5, true),
            d("2026-11-13")
        );
        // Added late: the next payroll from today.
        assert_eq!(job_payroll(done, d("2026-12-01"), 5, true), d("2026-12-04"));
        assert_eq!(
            job_payroll(done, d("2026-12-01"), 5, false),
            d("2026-12-04")
        );
    }

    #[test]
    fn each_work_has_its_position() {
        assert_eq!(position_of("template"), TEMPLATE_TECHNICIAN);
        assert_eq!(position_of("install"), INSTALLER);
    }
}
