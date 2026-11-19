//! The dashboard's "Finish setting up" checklist (ENG-90).
//!
//! What a company still has to configure: a company that signed itself up
//! starts with none of the integrations Granite Manager used to set up by
//! hand.
//!
//! - `GET /v1/company/setup-checklist`        → the items, done or not
//! - `PUT /v1/company/setup-checklist/{key}`  → tick (or untick) an item by hand
//!
//! Each item is seen done from the company's data, or ticked by a manager
//! (done, or not needed: `company_setup_ticks`). An item the company cannot
//! do itself (`how: contact`) says to contact Granite Manager. Managers and
//! superusers only (`BillingManager`).

use crate::auth::BillingManager;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get_setup_checklist))
        .routes(routes!(tick_setup_item))
}

/// Where Granite Manager is reached for the items a company cannot do itself.
pub const CONTACT_EMAIL: &str = "support@granite-manager.com";

/// Every item key, in the order shown.
pub const ITEM_KEYS: [&str; 10] = [
    "team",
    "company_profile",
    "web_address",
    "phone_line",
    "texting_registration",
    "company_email",
    "gmail",
    "lead_form",
    "stones_website",
    "quickbooks",
];

/// Who does an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SetupHow {
    /// The company does it itself, on the page `href` opens.
    #[serde(rename = "self")]
    SelfServe,
    /// Granite Manager does it: contact them (`contact_email`).
    Contact,
}

/// One thing to configure.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SetupItem {
    /// Item key, for `PUT /v1/company/setup-checklist/{key}`.
    pub key: String,
    /// What to configure.
    pub title: String,
    /// What it gives, or where it stands.
    pub hint: String,
    /// Done: seen in the company's data, or ticked by hand.
    pub done: bool,
    /// Seen done in the company's data.
    pub auto_done: bool,
    /// Ticked by a manager (done, or not needed).
    pub ticked: bool,
    /// `self` or `contact`.
    pub how: SetupHow,
    /// The page to do it on (`self` items), e.g. `/admin/users`; null for
    /// `contact` items.
    pub href: Option<String>,
    /// Not every company needs it; it does not count toward `done_count`.
    pub optional: bool,
}

/// `GET /v1/company/setup-checklist`.
#[derive(Debug, Serialize, ToSchema)]
pub struct SetupChecklist {
    /// The items, in order.
    pub items: Vec<SetupItem>,
    /// Required items done.
    pub done_count: usize,
    /// Required items in all.
    pub total_count: usize,
    /// Granite Manager's address for the `contact` items.
    pub contact_email: String,
}

#[derive(sqlx::FromRow)]
struct Facts {
    subdomain: Option<String>,
    requested_subdomain: Option<String>,
    domain: Option<String>,
    has_logo: bool,
    has_tax: bool,
    team: bool,
    telnyx: bool,
    tendlc_status: Option<String>,
    gmail: bool,
    has_lead: bool,
    stones_cache: Option<String>,
}

fn filled(value: Option<&String>) -> Option<&str> {
    value.map(|v| v.trim()).filter(|v| !v.is_empty())
}

struct Draft {
    key: &'static str,
    title: &'static str,
    hint: String,
    auto_done: bool,
    how: SetupHow,
    href: Option<&'static str>,
    optional: bool,
}

const fn item(key: &'static str, title: &'static str, how: SetupHow) -> Draft {
    Draft {
        key,
        title,
        hint: String::new(),
        auto_done: false,
        how,
        href: None,
        optional: false,
    }
}

fn tendlc_hint(status: Option<&str>) -> &'static str {
    match status {
        Some("approved") => "Approved: your numbers can text customers",
        Some("sent" | "campaign_pending") => "Sent: waiting for the carriers to approve it",
        Some("ready") => "Filled in: Granite Manager sends it for you",
        Some("rejected" | "suspended") => "Needs changes: open it to see why",
        _ => "Fill in your business details so carriers let your texts through",
    }
}

fn drafts(facts: &Facts, base_domain: &str, quickbooks_on: bool, qbo: bool) -> Vec<Draft> {
    let requested = filled(facts.requested_subdomain.as_ref());
    let live = filled(facts.subdomain.as_ref());
    let domain = filled(facts.domain.as_ref());
    let stones = facts
        .stones_cache
        .as_deref()
        .and_then(|json| serde_json::from_str::<Value>(json).ok())
        .and_then(|cache| cache.get("stones_website").and_then(Value::as_bool))
        .unwrap_or(false);
    let mut items = vec![
        Draft {
            hint: "Add your sales reps, installers and office staff".into(),
            auto_done: facts.team,
            href: Some("/admin/users"),
            ..item("team", "Add your team", SetupHow::SelfServe)
        },
        Draft {
            hint: "Your logo and sales tax go on quotes and invoices".into(),
            auto_done: facts.has_logo && facts.has_tax,
            href: Some("/admin/company"),
            ..item(
                "company_profile",
                "Add your logo and sales tax",
                SetupHow::SelfServe,
            )
        },
        Draft {
            hint: match (live, requested) {
                (Some(live), _) => format!("{live}.{base_domain}"),
                (None, Some(requested)) => {
                    format!("{requested}.{base_domain}: Granite Manager turns it on for you")
                }
                (None, None) => format!("yourcompany.{base_domain}, with your own sign-in page"),
            },
            auto_done: live.is_some(),
            ..item("web_address", "Your own web address", SetupHow::Contact)
        },
        Draft {
            hint: "A business number for calls and texts in Granite Manager".into(),
            auto_done: facts.telnyx,
            ..item(
                "phone_line",
                "Phone line (calls and texts)",
                SetupHow::Contact,
            )
        },
    ];
    if facts.telnyx {
        items.push(Draft {
            hint: tendlc_hint(facts.tendlc_status.as_deref()).into(),
            auto_done: facts.tendlc_status.as_deref() == Some("approved"),
            href: Some("/employee/texting-registration"),
            ..item(
                "texting_registration",
                "Register for business texting (10DLC)",
                SetupHow::SelfServe,
            )
        });
    }
    items.push(Draft {
        hint: domain.map_or_else(
            || "Send and receive email as you@yourcompany.com".to_string(),
            |domain| format!("Email from {domain}"),
        ),
        auto_done: domain.is_some(),
        ..item("company_email", "Company email domain", SetupHow::Contact)
    });
    if domain.is_none() {
        items.push(Draft {
            hint: "Until your email domain is set up, send email from your own Gmail".into(),
            auto_done: facts.gmail,
            href: Some("/employee/user#gmail-send"),
            optional: true,
            ..item("gmail", "Connect your Gmail", SetupHow::SelfServe)
        });
    }
    items.push(Draft {
        hint: "Leads from your website's contact form come straight in".into(),
        auto_done: facts.has_lead,
        ..item("lead_form", "Website contact form", SetupHow::Contact)
    });
    items.push(Draft {
        hint: "Customers browse your stones on your own website".into(),
        auto_done: stones,
        ..item(
            "stones_website",
            "Stones on your website",
            SetupHow::Contact,
        )
    });
    if quickbooks_on {
        items.push(Draft {
            hint: "Send invoices and sales to QuickBooks".into(),
            auto_done: qbo,
            href: Some("/admin/company#company-quickbooks"),
            optional: true,
            ..item("quickbooks", "Connect QuickBooks", SetupHow::SelfServe)
        });
    }
    items
}

/// The checklist of `company_id`, for `user_id` (the Gmail item is theirs).
pub async fn checklist(
    state: &AppState,
    company_id: i32,
    user_id: i32,
) -> ApiResult<SetupChecklist> {
    let pool = &state.pool;
    let facts: Facts = sqlx::query_as(
        "SELECT c.subdomain, c.requested_subdomain, c.domain,
                COALESCE(TRIM(c.logo_url), '') <> '' AS has_logo,
                c.state_taxes IS NOT NULL AS has_tax,
                EXISTS (SELECT 1 FROM users u JOIN users_positions up ON up.user_id = u.id
                         WHERE up.company_id = c.id AND u.is_deleted = 0 AND u.id <> ?
                           AND up.position_id NOT IN (8, 9)) AS team,
                EXISTS (SELECT 1 FROM telnyx_company_config t
                         WHERE t.company_id = c.id AND t.enabled <> 0) AS telnyx,
                (SELECT r.status FROM tendlc_registrations r WHERE r.company_id = c.id LIMIT 1)
                    AS tendlc_status,
                EXISTS (SELECT 1 FROM gmail_user_logins g WHERE g.user_id = ?) AS gmail,
                EXISTS (SELECT 1 FROM customers cu
                         WHERE cu.company_id = c.id AND cu.source = 'leads'
                           AND cu.referral_source IS NOT NULL AND cu.deleted_at IS NULL) AS has_lead,
                (SELECT CAST(s.auto_cache AS CHAR) FROM company_setup_checklist s
                  WHERE s.company_id = c.id) AS stones_cache
           FROM company c WHERE c.id = ?",
    )
    .bind(user_id)
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?
    .ok_or(ApiError::NotFound)?;
    let features = crate::routes::company::queries::company_features(pool, company_id).await?;
    let ticked: Vec<String> =
        sqlx::query_scalar("SELECT item_key FROM company_setup_ticks WHERE company_id = ?")
            .bind(company_id)
            .fetch_all(pool)
            .await?;
    let base = crate::setup_checklist::base_domain(state);
    let items: Vec<SetupItem> = drafts(
        &facts,
        &base,
        features.quickbooks_enabled,
        features.quickbooks_connected,
    )
    .into_iter()
    .map(|draft| {
        let ticked = ticked.iter().any(|key| key == draft.key);
        SetupItem {
            key: draft.key.into(),
            title: draft.title.into(),
            hint: draft.hint,
            done: draft.auto_done || ticked,
            auto_done: draft.auto_done,
            ticked,
            how: draft.how,
            href: draft.href.map(str::to_string),
            optional: draft.optional,
        }
    })
    .collect();
    let required = items.iter().filter(|item| !item.optional);
    Ok(SetupChecklist {
        done_count: required.clone().filter(|item| item.done).count(),
        total_count: required.count(),
        items,
        contact_email: CONTACT_EMAIL.into(),
    })
}

/// Get what the company still has to set up
///
/// The dashboard's "Finish setting up" checklist: the team, the logo and
/// sales tax, the web address, the phone line, the texting registration
/// (once there is a phone line), the email domain (or the user's Gmail
/// meanwhile), the website contact form, the stones on the website and
/// QuickBooks (when switched on). Each item is done when the company's data
/// shows it or a manager ticked it. Items with `how: contact` are done by
/// Granite Manager: the page says to contact them at `contact_email`.
/// Managers and superusers only.
#[crate::openapi::path(
    get,
    path = "/v1/company/setup-checklist",
    operation_id = "get_company_setup_checklist",
    tag = "company",
    responses(
        (status = 200, description = "The checklist", body = SetupChecklist),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a manager of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_setup_checklist(
    State(state): State<AppState>,
    BillingManager(user): BillingManager,
) -> ApiResult<Json<SetupChecklist>> {
    Ok(Json(checklist(&state, user.company_id, user.id).await?))
}

/// Body of `PUT /v1/company/setup-checklist/{key}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetupTick {
    /// True: done (or not needed); false: take the tick off.
    pub ticked: bool,
}

/// Tick a setup item by hand
///
/// Marks the item done (or not needed) for the whole company, or takes the
/// tick off; an item the company's data shows done stays done. Answers the
/// checklist. 404 for an unknown key. Managers and superusers only.
#[crate::openapi::path(
    put,
    path = "/v1/company/setup-checklist/{key}",
    operation_id = "tick_company_setup_item",
    tag = "company",
    params(("key" = String, Path, description = "Item key, e.g. `lead_form`")),
    request_body = SetupTick,
    responses(
        (status = 200, description = "Saved; the checklist", body = SetupChecklist),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a manager of the company", body = ErrorBody),
        (status = 404, description = "Unknown item", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn tick_setup_item(
    State(state): State<AppState>,
    BillingManager(user): BillingManager,
    Path(key): Path<String>,
    JsonBody(input): JsonBody<SetupTick>,
) -> ApiResult<(StatusCode, Json<SetupChecklist>)> {
    if !ITEM_KEYS.contains(&key.as_str()) {
        return Err(ApiError::NotFound);
    }
    if input.ticked {
        sqlx::query(
            "INSERT INTO company_setup_ticks (company_id, item_key, ticked_by_user_id)
             VALUES (?, ?, ?)
             ON DUPLICATE KEY UPDATE ticked_by_user_id = VALUES(ticked_by_user_id),
                                     ticked_at = CURRENT_TIMESTAMP",
        )
        .bind(user.company_id)
        .bind(&key)
        .bind(user.id)
        .execute(&state.pool)
        .await?;
    } else {
        sqlx::query("DELETE FROM company_setup_ticks WHERE company_id = ? AND item_key = ?")
            .bind(user.company_id)
            .bind(&key)
            .execute(&state.pool)
            .await?;
    }
    Ok((
        StatusCode::OK,
        Json(checklist(&state, user.company_id, user.id).await?),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bare() -> Facts {
        Facts {
            subdomain: None,
            requested_subdomain: Some("acme".into()),
            domain: None,
            has_logo: false,
            has_tax: false,
            team: false,
            telnyx: false,
            tendlc_status: None,
            gmail: false,
            has_lead: false,
            stones_cache: None,
        }
    }

    fn keys(items: &[Draft]) -> Vec<&str> {
        items.iter().map(|d| d.key).collect()
    }

    #[test]
    fn a_new_company_has_everything_to_do() {
        let items = drafts(&bare(), "granite-manager.com", true, false);
        assert_eq!(
            keys(&items),
            [
                "team",
                "company_profile",
                "web_address",
                "phone_line",
                "company_email",
                "gmail",
                "lead_form",
                "stones_website",
                "quickbooks"
            ]
        );
        assert!(items.iter().all(|d| !d.auto_done));
        assert_eq!(
            items[2].hint,
            "acme.granite-manager.com: Granite Manager turns it on for you"
        );
        let contact: Vec<&str> = items
            .iter()
            .filter(|d| d.how == SetupHow::Contact)
            .map(|d| d.key)
            .collect();
        assert_eq!(
            contact,
            [
                "web_address",
                "phone_line",
                "company_email",
                "lead_form",
                "stones_website"
            ]
        );
        assert!(
            items
                .iter()
                .all(|d| (d.how == SetupHow::SelfServe) == d.href.is_some())
        );
    }

    #[test]
    fn a_set_up_company_is_done() {
        let facts = Facts {
            subdomain: Some("acme".into()),
            domain: Some("acme.com".into()),
            has_logo: true,
            has_tax: true,
            team: true,
            telnyx: true,
            tendlc_status: Some("approved".into()),
            has_lead: true,
            stones_cache: Some(r#"{"stones_website": true}"#.into()),
            ..bare()
        };
        let items = drafts(&facts, "granite-manager.com", false, false);
        // Texting registration once there is a line; no Gmail with a domain;
        // no QuickBooks while it is switched off.
        assert_eq!(
            keys(&items),
            [
                "team",
                "company_profile",
                "web_address",
                "phone_line",
                "texting_registration",
                "company_email",
                "lead_form",
                "stones_website"
            ]
        );
        assert!(items.iter().all(|d| d.auto_done));
        assert!(items.iter().all(|d| ITEM_KEYS.contains(&d.key)));
    }

    #[test]
    fn the_texting_registration_says_where_it_stands() {
        let facts = Facts {
            telnyx: true,
            tendlc_status: Some("sent".into()),
            ..bare()
        };
        let items = drafts(&facts, "granite-manager.com", false, false);
        let tendlc = items
            .iter()
            .find(|d| d.key == "texting_registration")
            .unwrap();
        assert!(!tendlc.auto_done);
        assert_eq!(tendlc.hint, "Sent: waiting for the carriers to approve it");
    }
}
