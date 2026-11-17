//! Employee CRM › Automations: whose automated emails and texts go out when a
//! deal of the caller lands on a list (`users.lead_automation`).
//!
//! - `GET /v1/me/lead-automation` → the caller's choice
//! - `PUT /v1/me/lead-automation` → change it
//!
//! The scheduling side is `common::crud::lead_automation`.

use crate::auth::EmployeeUser;
use crate::error::{ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::routes::company::queries::company_features;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use common::crud::lead_automation::{COMPANY_TEMPLATE_FOR_REP, OWN_TEMPLATE};
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;

/// Whose automated templates a sales rep's deals get.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum LeadAutomationMode {
    /// The company's automated email and SMS templates (Admin › Automations)
    /// that go to this rep's deals. Every user starts here.
    #[default]
    Company,
    /// Only the rep's own automated templates (personal templates with a
    /// trigger list); the company's do not go to their deals.
    Own,
}

impl LeadAutomationMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Company => "company",
            Self::Own => "own",
        }
    }

    fn parse(raw: &str) -> Self {
        if raw == "own" {
            Self::Own
        } else {
            Self::Company
        }
    }
}

/// The caller's automation choice; also the body of `PUT /v1/me/lead-automation`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LeadAutomation {
    /// `company` or `own`.
    pub mode: LeadAutomationMode,
}

/// The user's choice (also part of `GET /v1/me/account`).
pub async fn mode_of(pool: &MySqlPool, user_id: i32) -> Result<LeadAutomationMode, sqlx::Error> {
    let raw: Option<String> =
        sqlx::query_scalar("SELECT CAST(lead_automation AS CHAR) FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    Ok(raw
        .as_deref()
        .map_or_else(LeadAutomationMode::default, LeadAutomationMode::parse))
}

/// Pending automatic sends of the side the caller just left are called off:
/// the company's on their deals when they take their own, their own when they
/// go back to the company's. Nothing is scheduled for deals already on a
/// list; the new choice applies from the next deal that lands on one.
async fn cancel_left_side(
    pool: &MySqlPool,
    user_id: i32,
    mode: LeadAutomationMode,
) -> Result<(), sqlx::Error> {
    for (queue, templates) in [
        ("scheduled_emails", "email_templates"),
        ("scheduled_sms", "cloudtalk_sms_templates"),
    ] {
        let (condition, reason) = match mode {
            LeadAutomationMode::Own => (
                "t.user_id IS NULL AND d.user_id = ?",
                "Sales rep uses their own automations",
            ),
            LeadAutomationMode::Company => {
                ("t.user_id = ?", "Sales rep uses the company automations")
            }
        };
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {queue} s
               JOIN deals d ON d.id = s.deal_id
               JOIN {templates} t ON t.id = s.template_id
                SET s.status = 'cancelled', s.error_message = ?
              WHERE s.status = 'pending' AND {condition}"
        )))
        .bind(reason)
        .bind(user_id)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Get whose automations the caller's deals get
///
/// `company` (the default): when a deal of the caller lands on a list (e.g.
/// Not Contacted Yet), the company's automated email and SMS templates for
/// that list go out (Admin › Automations; those naming no sales reps, or
/// naming the caller). `own`: only the caller's own automated templates do —
/// personal templates with a `lead_list_id` (`GET /v1/email-templates`,
/// `GET /v1/sms-templates`, `scope: personal`).
#[crate::openapi::path(
    get,
    path = "/v1/me/lead-automation",
    operation_id = "get_lead_automation",
    tag = "templates",
    responses(
        (status = 200, description = "The caller's choice", body = LeadAutomation),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<LeadAutomation>> {
    let mode = mode_of(&state.pool, user.id).await?;
    Ok(Json(LeadAutomation { mode }))
}

/// Choose whose automations the caller's deals get
///
/// `company` or `own` (see `GET /v1/me/lead-automation`). Changing it calls
/// off the pending automatic sends of the side left behind (the company's on
/// the caller's deals, or the caller's own); deals already on a list get
/// nothing new, the choice applies to deals that land on a list from now on.
/// Returns the saved choice.
#[crate::openapi::path(
    put,
    path = "/v1/me/lead-automation",
    operation_id = "set_lead_automation",
    tag = "templates",
    request_body = LeadAutomation,
    responses(
        (status = 200, description = "Saved", body = LeadAutomation),
        (status = 400, description = "Unknown mode", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<LeadAutomation>,
) -> ApiResult<Json<LeadAutomation>> {
    let pool = &state.pool;
    let before = mode_of(pool, user.id).await?;
    if before != input.mode {
        sqlx::query("UPDATE users SET lead_automation = ? WHERE id = ?")
            .bind(input.mode.as_str())
            .bind(user.id)
            .execute(pool)
            .await?;
        cancel_left_side(pool, user.id, input.mode).await?;
    }
    Ok(Json(input))
}

/// Which kind of template an automation sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum AutomationChannel {
    /// An email template (`/v1/email-templates`).
    Email,
    /// A text template (`/v1/sms-templates`).
    Sms,
}

/// One automated email or text: it goes out by itself when a deal lands on
/// its list.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AutomationItem {
    /// Email or text.
    pub channel: AutomationChannel,
    /// The template's id: edit or delete it with `/v1/email-templates/{id}`
    /// or `/v1/sms-templates/{id}` by `channel`.
    pub template_id: i32,
    /// The template's name.
    pub name: String,
    /// The deal list that starts it.
    pub list_id: i32,
    /// That list's name; null when the list was deleted.
    pub list_name: Option<String>,
    /// The name of the list's group (pipeline); null when the list or the
    /// group was deleted.
    pub group_name: Option<String>,
    /// How long after the deal lands on the list it goes out, in minutes.
    pub delay_minutes: i32,
}

/// `GET /v1/me/lead-automation/items`: the caller's automations on each side.
#[derive(Debug, Serialize, ToSchema)]
pub struct LeadAutomationItems {
    /// The company's automations that go to the caller's deals while they
    /// use the company's (mode `company`): those naming no sales reps, or
    /// naming the caller.
    pub company: Vec<AutomationItem>,
    /// The caller's own automations, used while their mode is `own`.
    pub own: Vec<AutomationItem>,
}

#[derive(sqlx::FromRow)]
struct AutomationRow {
    own: bool,
    template_id: i32,
    name: Option<String>,
    list_id: i32,
    list_name: Option<String>,
    group_name: Option<String>,
    delay_minutes: i32,
}

/// The automated templates of one table on both sides, with their lists.
async fn automation_rows(
    pool: &MySqlPool,
    table: &str,
    delay_minutes: &str,
    company_id: i32,
    user_id: i32,
) -> Result<Vec<(bool, AutomationItem)>, sqlx::Error> {
    let rows: Vec<AutomationRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT t.user_id IS NOT NULL AS own, t.id AS template_id, t.{name} AS name,
                    t.lead_list_id AS list_id, dl.name AS list_name, gl.name AS group_name,
                    CAST({delay_minutes} AS SIGNED) AS delay_minutes
               FROM {table} t
               LEFT JOIN deals_list dl ON dl.id = t.lead_list_id AND dl.deleted_at IS NULL
               LEFT JOIN groups_list gl ON gl.id = dl.group_id AND gl.deleted_at IS NULL
              WHERE t.company_id = ? AND t.deleted_at IS NULL AND t.lead_list_id IS NOT NULL
                AND (({OWN_TEMPLATE}) OR ({COMPANY_TEMPLATE_FOR_REP}))
              ORDER BY t.id",
        name = if table == "email_templates" {
            "template_name"
        } else {
            "name"
        },
    )))
    .bind(company_id)
    .bind(user_id)
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    let channel = if table == "email_templates" {
        AutomationChannel::Email
    } else {
        AutomationChannel::Sms
    };
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.own,
                AutomationItem {
                    channel,
                    template_id: row.template_id,
                    name: row.name.unwrap_or_default(),
                    list_id: row.list_id,
                    list_name: row.list_name,
                    group_name: row.group_name,
                    delay_minutes: row.delay_minutes,
                },
            )
        })
        .collect())
}

/// List the automations of the caller's deals
///
/// Every automated email and text (a template with a deal list) that can go
/// out for the caller's deals, on both sides: `company`, the company's ones
/// that reach the caller (Admin › Automations, naming no sales reps or naming
/// the caller), and `own`, the caller's personal ones. Which side is in use is
/// `GET /v1/me/lead-automation`. Texts are left out until the company has a
/// texting provider. Deleted templates and templates without a list are
/// hidden. Edit or delete one with the email or SMS template routes named by
/// `channel` and `template_id`.
#[crate::openapi::path(
    get,
    path = "/v1/me/lead-automation/items",
    operation_id = "list_lead_automation_items",
    tag = "templates",
    responses(
        (status = 200, description = "The caller's automations on each side", body = LeadAutomationItems),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn items(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<LeadAutomationItems>> {
    let pool = &state.pool;
    let mut rows = automation_rows(
        pool,
        "email_templates",
        "COALESCE(t.hour_delay, 0) * 60",
        user.company_id,
        user.id,
    )
    .await?;
    // As `GET /v1/sms-templates`: no texts until the company can text.
    let features = company_features(pool, user.company_id).await?;
    if features.cloudtalk || features.ringcentral || features.telnyx_sms {
        rows.extend(
            automation_rows(
                pool,
                "cloudtalk_sms_templates",
                "COALESCE(t.hour_delay, 0) * 60 + COALESCE(t.minute_delay, 0)",
                user.company_id,
                user.id,
            )
            .await?,
        );
    }
    let (own, company): (Vec<_>, Vec<_>) = rows.into_iter().partition(|(own, _)| *own);
    Ok(Json(LeadAutomationItems {
        company: company.into_iter().map(|(_, item)| item).collect(),
        own: own.into_iter().map(|(_, item)| item).collect(),
    }))
}
