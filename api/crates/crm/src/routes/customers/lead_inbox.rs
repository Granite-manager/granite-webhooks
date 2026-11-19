//! `GET /v1/customers/lead-inbox`: the phone app's lead notifications (Deals
//! › ⋮ › Notifications).
//!
//! A sales rep sees the leads assigned to them in the last 14 days. A sales
//! manager (or an admin) also sees the company's leads of the last 14 days
//! that have no rep yet, with the sales reps and this month's lead count of
//! each, as the Telegram lead alert's buttons show them; assigning goes
//! through `PUT /v1/customers/{id}/sales-rep`. The managers see every lead
//! assigned in the window, not only their own.
//!
//! Everyone, a sales rep too, gets `team`: how many leads each of the
//! company's sales reps got this month, for the screen a lead push opens.
//!
//! The website's Leads page (Deals › Leads, sales managers only) reads the
//! same inbox, with each lead's email, source and form message, and the
//! company's pipelines (`groups`) to choose where an assigned lead goes.

use crate::auth::EmployeeUser;
use crate::error::{ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::MySqlPool;

/// `positions.id` of a sales rep and of a sales manager.
const SALES_REP: i32 = 1;
const SALES_MANAGER: i32 = 2;
/// How far back the inbox goes.
const DAYS: i32 = 14;
/// The most leads of each list.
const LIMIT: i64 = 100;
/// The onboarding sample customer is never a lead here.
const SAMPLE_CUSTOMER_ID: i32 = 990_000_021;
const SAMPLE_CUSTOMER_NAME: &str = "Onboarding Sample Customer";

/// A lead of the inbox.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct InboxLead {
    /// The lead's customer id.
    pub customer_id: i32,
    /// The customer's name (null when the lead has none).
    pub name: Option<String>,
    /// The customer's phone (null when the lead has none).
    pub phone: Option<String>,
    /// When the lead came in.
    pub created_at: Option<DateTime<Utc>>,
    /// When it was first given a rep (null while it has none).
    pub assigned_at: Option<DateTime<Utc>>,
    /// The sales rep's user id; null while the lead has no rep.
    pub sales_rep_id: Option<i32>,
    /// The sales rep's name; null while the lead has no rep.
    pub sales_rep_name: Option<String>,
    /// Its newest live deal, the one the app opens.
    pub deal_id: Option<i64>,
    /// The customer's primary email (null when none).
    pub email: Option<String>,
    /// How they found the company ("Google", "Facebook"…; null when unknown).
    pub referral_source: Option<String>,
    /// What the customer wrote in the lead form (null when nothing).
    pub your_message: Option<String>,
    /// The customer's address (null when none).
    pub address: Option<String>,
    /// `hot`, `medium` or `cold`; null when not rated.
    pub customer_temperature: Option<String>,
    /// A second phone; null when the lead has none.
    pub phone_2: Option<String>,
    /// The customer's company; null when the lead has none.
    pub company_name: Option<String>,
    /// City; null when the lead has none.
    pub city: Option<String>,
    /// Zip code; null when the lead has none.
    pub postal_code: Option<String>,
    /// What they remodel ("Kitchen"…); null when the lead has none.
    pub remodal_type: Option<String>,
    /// How big the project is; null when the lead has none.
    pub project_size: Option<String>,
    /// When they want to start; null when the lead has none.
    pub when_start: Option<String>,
    /// The best time to contact them; null when the lead has none.
    pub contact_time: Option<String>,
    /// Whether the old countertops are removed; null when the lead has none.
    pub remove_and_dispose: Option<String>,
    /// Their answer to the offer question; null when the lead has none.
    pub improve_offer: Option<String>,
    /// The sink they chose; null when the lead has none.
    pub sink: Option<String>,
    /// The stove they chose; null when the lead has none.
    pub kitchen_stove: Option<String>,
    /// The backsplash they chose; null when the lead has none.
    pub backsplash: Option<String>,
    /// Other answers of the form (a Facebook lead); null when the lead has none.
    pub details: Option<String>,
    /// The file they attached (a link); null when the lead has none.
    pub attached_file: Option<String>,
    /// The form the lead came from; null when the lead has none.
    pub form_name: Option<String>,
    /// The ad campaign; null when the lead has none.
    pub compaign_name: Option<String>,
    /// The ad set; null when the lead has none.
    pub adset_name: Option<String>,
    /// The ad; null when the lead has none.
    pub ad_name: Option<String>,
}

/// A live pipeline (group) of the company, for the website's Leads page.
#[derive(Debug, Serialize, ToSchema)]
pub struct InboxGroup {
    /// The group id (`PUT /v1/customers/{id}/sales-rep` takes it as `group_id`).
    pub id: i32,
    /// Its name.
    pub name: Option<String>,
    /// The pipeline new leads go to ("Assign leads here").
    pub is_default: bool,
    /// Its live lists (stages) in board order; `PUT /v1/customers/{id}/sales-rep`
    /// takes one as `list_id`.
    pub lists: Vec<InboxList>,
}

/// A live list (stage) of a pipeline, for the website's Leads page.
#[derive(Debug, Serialize, ToSchema)]
pub struct InboxList {
    /// The list id.
    pub id: i32,
    /// Its name.
    pub name: Option<String>,
}

/// A sales rep of the company and their lead count this month.
#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct InboxRep {
    /// The sales rep's user id (`PUT /v1/customers/{id}/sales-rep` takes it).
    pub id: i32,
    /// Their name (null when the account has none).
    pub name: Option<String>,
    /// Leads given to them this month (the Telegram button's count).
    pub month_leads: i64,
}

/// `GET /v1/customers/lead-inbox`.
#[derive(Debug, Serialize, ToSchema)]
pub struct LeadInbox {
    /// A sales manager or an admin: `unassigned` and `reps` are filled in.
    pub is_manager: bool,
    /// Leads of the last 14 days without a rep, newest first (managers).
    pub unassigned: Vec<InboxLead>,
    /// Leads given a rep in the last 14 days, newest first: mine, or the
    /// company's for a manager.
    pub assigned: Vec<InboxLead>,
    /// The company's sales reps to assign a lead to (managers only; empty
    /// for a sales rep).
    pub reps: Vec<InboxRep>,
    /// Every sales rep of the company with the leads given to them this
    /// month (since the 1st, server time), sorted by name. Filled in for
    /// everyone, a sales rep too, so they can compare their count with the
    /// team's; the same numbers as `reps`.
    pub team: Vec<InboxRep>,
    /// The company's live pipelines, oldest first (managers only; empty for
    /// a sales rep). When none is the default, the Leads page asks which
    /// list a lead goes to.
    pub groups: Vec<InboxGroup>,
}

/// List my lead notifications
///
/// The leads assigned to me in the last 14 days; for a sales manager or an
/// admin, also the company's leads of the last 14 days that have no rep yet
/// and the sales reps to give them to (with this month's lead count each).
/// Everyone also gets `team`: each sales rep of the company and the leads
/// given to them this month.
#[crate::openapi::path(
    get,
    path = "/v1/customers/lead-inbox",
    operation_id = "lead_inbox",
    tag = "customers",
    responses(
        (status = 200, description = "My lead notifications", body = LeadInbox),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn lead_inbox(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<LeadInbox>> {
    let pool = &state.pool;
    let is_manager = user.is_admin || user.is_superuser || {
        let found: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM users_positions
              WHERE user_id = ? AND company_id = ? AND position_id = ? LIMIT 1",
        )
        .bind(user.id)
        .bind(user.company_id)
        .bind(SALES_MANAGER)
        .fetch_optional(pool)
        .await?;
        found.is_some()
    };
    let assigned = if is_manager {
        leads(pool, user.company_id, Some(None), "c.assigned_date").await?
    } else {
        leads(
            pool,
            user.company_id,
            Some(Some(user.id)),
            "c.assigned_date",
        )
        .await?
    };
    let team = reps(pool, user.company_id).await?;
    let (unassigned, reps, groups) = if is_manager {
        (
            leads(pool, user.company_id, None, "c.created_date").await?,
            team.clone(),
            groups(pool, user.company_id).await?,
        )
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };
    Ok(Json(LeadInbox {
        is_manager,
        unassigned,
        assigned,
        reps,
        team,
        groups,
    }))
}

/// Live, valid leads of the window by `date`: `rep` None = without a rep,
/// Some(None) = with any rep, Some(Some(id)) = with that rep.
async fn leads(
    pool: &MySqlPool,
    company_id: i32,
    rep: Option<Option<i32>>,
    date: &'static str,
) -> sqlx::Result<Vec<InboxLead>> {
    let who = match rep {
        None => " AND c.sales_rep IS NULL",
        Some(None) => " AND c.sales_rep IS NOT NULL",
        Some(Some(_)) => " AND c.sales_rep = ?",
    };
    let sql = format!(
        "SELECT c.id AS customer_id, c.name, c.phone, c.created_date AS created_at,
                c.assigned_date AS assigned_at, c.sales_rep AS sales_rep_id,
                u.name AS sales_rep_name,
                -- deals.id is BIGINT UNSIGNED, which decodes only as u64.
                (SELECT CAST(d.id AS SIGNED) FROM deals d
                  WHERE d.customer_id = c.id AND d.deleted_at IS NULL
                  ORDER BY d.id DESC LIMIT 1) AS deal_id,
                e.email, c.referral_source, c.your_message, c.address,
                CAST(c.customer_temperature AS CHAR) AS customer_temperature,
                CAST(c.phone_2 AS CHAR) AS phone_2, CAST(c.company_name AS CHAR) AS company_name,
                CAST(c.city AS CHAR) AS city, CAST(c.postal_code AS CHAR) AS postal_code,
                CAST(c.remodal_type AS CHAR) AS remodal_type, CAST(c.project_size AS CHAR) AS project_size,
                CAST(c.when_start AS CHAR) AS when_start, CAST(c.contact_time AS CHAR) AS contact_time,
                CAST(c.remove_and_dispose AS CHAR) AS remove_and_dispose, CAST(c.improve_offer AS CHAR) AS improve_offer,
                CAST(c.sink AS CHAR) AS sink, CAST(c.kitchen_stove AS CHAR) AS kitchen_stove,
                CAST(c.backsplash AS CHAR) AS backsplash, CAST(c.details AS CHAR) AS details,
                CAST(c.attached_file AS CHAR) AS attached_file, CAST(c.form_name AS CHAR) AS form_name,
                CAST(c.compaign_name AS CHAR) AS compaign_name, CAST(c.adset_name AS CHAR) AS adset_name,
                CAST(c.ad_name AS CHAR) AS ad_name
           FROM customers c
           LEFT JOIN users u ON u.id = c.sales_rep
           LEFT JOIN customers_emails e ON e.id = c.email_id AND e.customer_id = c.id
          WHERE c.company_id = ? AND c.source = 'leads' AND c.deleted_at IS NULL
            AND (c.invalid_lead IS NULL OR c.invalid_lead = '')
            AND c.id != ? AND (c.name IS NULL OR c.name != ?)
            AND {date} >= NOW() - INTERVAL ? DAY{who}
          ORDER BY {date} DESC, c.id DESC
          LIMIT ?"
    );
    let mut query = sqlx::query_as::<_, InboxLead>(sqlx::AssertSqlSafe(sql))
        .bind(company_id)
        .bind(SAMPLE_CUSTOMER_ID)
        .bind(SAMPLE_CUSTOMER_NAME)
        .bind(DAYS);
    if let Some(Some(id)) = rep {
        query = query.bind(id);
    }
    query.bind(LIMIT).fetch_all(pool).await
}

/// The company's live pipelines, oldest first, each with its live lists.
async fn groups(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Vec<InboxGroup>> {
    let rows: Vec<(i32, Option<String>, Option<bool>)> = sqlx::query_as(
        "SELECT id, name, is_default FROM groups_list
          WHERE company_id = ? AND deleted_at IS NULL
          ORDER BY created_at, id",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    let lists: Vec<(i32, i32, Option<String>)> = sqlx::query_as(
        "SELECT dl.group_id, dl.id, dl.name FROM deals_list dl
           JOIN groups_list g ON g.id = dl.group_id
          WHERE g.company_id = ? AND g.deleted_at IS NULL AND dl.deleted_at IS NULL
          ORDER BY dl.position, dl.id",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, name, default)| InboxGroup {
            id,
            name,
            is_default: default.unwrap_or(false),
            lists: lists
                .iter()
                .filter(|(group, _, _)| *group == id)
                .map(|(_, list, name)| InboxList {
                    id: *list,
                    name: name.clone(),
                })
                .collect(),
        })
        .collect())
}

/// The company's sales reps with this month's leads, as the Telegram lead
/// alert counts them.
async fn reps(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Vec<InboxRep>> {
    sqlx::query_as(
        "SELECT u.id, u.name, COUNT(c.id) AS month_leads
           FROM users u
           JOIN users_positions up ON up.user_id = u.id AND up.company_id = u.company_id
           LEFT JOIN customers c ON c.sales_rep = u.id AND c.source = 'leads'
                AND c.assigned_date >= DATE_FORMAT(NOW(), '%Y-%m-01')
                AND c.company_id = u.company_id AND c.deleted_at IS NULL
          WHERE u.company_id = ? AND u.is_deleted = 0 AND up.position_id = ?
          GROUP BY u.id, u.name
          ORDER BY u.name, u.id",
    )
    .bind(company_id)
    .bind(SALES_REP)
    .fetch_all(pool)
    .await
}
