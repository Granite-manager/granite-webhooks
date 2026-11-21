//! The sidebar badges and desktop alerts in one request.
//!
//! - `GET /v1/me/badges` → the parts asked for: the inbox badge, the texts
//!   badge, the texts to alert about, the sales orders to confirm, unread
//!   support answers
//!
//! Every open tab polls these each minute. One request instead of one per
//! badge keeps the Lambdas and database connections of the poll to one. Each
//! part reads only our database, so a slow outside service never holds the
//! badges up (the notification bell, which reads `CloudTalk` and
//! `RingCentral`, stays `GET /v1/notifications`). Each part is computed the
//! way its own route computes it, and those routes stay.

use crate::auth::EmployeeAccess;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::openapi::{IntoParams, OpenApiRouter, ToSchema, routes};
use crate::routes::sms::provider::SmsProvider;
use crate::routes::sms::schemas::DesktopAlert;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(get_badges))
}

/// A part of `GET /v1/me/badges`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BadgePart {
    /// The inbox badge, as `GET /v1/emails/unread`.
    EmailsUnread,
    /// The texts badge, as `GET /v1/sms/mine/unread-count`.
    SmsUnread,
    /// The texts to alert about, as `GET /v1/sms/mine/desktop-alerts`.
    SmsAlerts,
    /// The sales orders to confirm, the count of `GET /v1/sales-orders`
    /// (pending).
    SalesOrders,
    /// The caller's support requests with an answer they have not read, as
    /// `unread` of `GET /v1/support/thread`.
    SupportUnread,
}

/// Query of `GET /v1/me/badges`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BadgesQuery {
    /// The parts to return; repeat the key for several
    /// (`include=emails_unread&include=sms_unread`). At least one.
    #[serde(default)]
    #[param(inline)]
    pub include: Vec<BadgePart>,
    /// The SMS page the `sms_alerts` open (the caller's inbox); default
    /// `telnyx`. Used only with `sms_alerts`.
    #[param(inline)]
    pub inbox: Option<SmsProvider>,
}

/// The inbox badge.
#[derive(Debug, Serialize, ToSchema)]
pub struct EmailBadge {
    /// Unread inbox threads; 0 when the company has no inbound email.
    pub count: i64,
    /// Newest unread message id, to notice new mail between polls; null when
    /// nothing is unread.
    pub latest_inbound_id: Option<i64>,
}

/// A badge count.
#[derive(Debug, Serialize, ToSchema)]
pub struct BadgeCount {
    /// How many.
    pub count: i64,
}

/// The texts to alert about.
#[derive(Debug, Serialize, ToSchema)]
pub struct SmsAlerts {
    /// One per phone (its latest unread text from the customer), newest
    /// first, at most 50.
    pub alerts: Vec<DesktopAlert>,
}

/// `GET /v1/me/badges`: the parts asked for; a part not asked for is absent.
#[derive(Debug, Default, Serialize, ToSchema)]
pub struct Badges {
    /// `emails_unread`: unread threads of the caller's inbox.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emails_unread: Option<EmailBadge>,
    /// `sms_unread`: the caller's conversations with unread texts over every
    /// provider they text through (a phone unread through two providers
    /// counts twice).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sms_unread: Option<BadgeCount>,
    /// `sms_alerts`: the caller's unread texts to show as desktop alerts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sms_alerts: Option<SmsAlerts>,
    /// `sales_orders`: the company's sales orders waiting to be confirmed.
    /// Absent, even when asked for, for someone whose positions do not open
    /// the Sales orders tab.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sales_orders: Option<BadgeCount>,
    /// `support_unread`: the caller's support requests with an answer of
    /// Granite Manager's support team they have not read (the Chat button's
    /// badge).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub support_unread: Option<BadgeCount>,
}

/// Get my badges
///
/// The sidebar badges and desktop alerts the CRM polls each minute, in one
/// request: name the parts with `include` (repeat the key). Each part is what
/// its own route returns: `emails_unread` as `GET /v1/emails/unread`,
/// `sms_unread` as `GET /v1/sms/mine/unread-count`, `sms_alerts` as
/// `GET /v1/sms/mine/desktop-alerts` (opening the `inbox` provider's page),
/// `sales_orders` the count of pending `GET /v1/sales-orders` (absent unless
/// the caller's positions open the Sales orders tab), `support_unread` the
/// caller's support requests with unread answers (as `unread` of
/// `GET /v1/support/thread`). Parts not asked for are
/// absent. Reads only the CRM's database; the notification bell is
/// `GET /v1/notifications`.
#[crate::openapi::path(
    get,
    path = "/v1/me/badges",
    operation_id = "get_my_badges",
    tag = "me",
    params(BadgesQuery),
    responses(
        (status = 200, description = "The parts asked for", body = Badges),
        (status = 400, description = "No part named in `include`, or an unknown one", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_badges(
    State(state): State<AppState>,
    EmployeeAccess { user, access }: EmployeeAccess,
    MultiQuery(query): MultiQuery<BadgesQuery>,
) -> ApiResult<Json<Badges>> {
    if query.include.is_empty() {
        return Err(ApiError::BadRequest(
            "include names at least one part: emails_unread, sms_unread, sms_alerts, sales_orders or support_unread"
                .into(),
        ));
    }
    let wants = |part| query.include.contains(&part);
    // One after the other: the Lambda's pool holds one connection.
    let mut badges = Badges::default();
    if wants(BadgePart::EmailsUnread) {
        let unread = crate::routes::emails::unread_badge(&state.pool, &user).await?;
        badges.emails_unread = Some(EmailBadge {
            count: unread.count,
            latest_inbound_id: unread.latest_inbound_id,
        });
    }
    if wants(BadgePart::SmsUnread) {
        let count = crate::routes::sms::my_unread(&state.pool, &user).await?;
        badges.sms_unread = Some(BadgeCount { count });
    }
    if wants(BadgePart::SmsAlerts) {
        let inbox = query.inbox.unwrap_or(SmsProvider::Telnyx);
        let alerts = crate::routes::sms::my_alerts(&state.pool, &user, inbox).await?;
        badges.sms_alerts = Some(SmsAlerts { alerts });
    }
    if wants(BadgePart::SalesOrders) && access.allows("/v1/sales-orders") {
        let count =
            crate::routes::sales_orders::pending_count(&state.pool, user.company_id).await?;
        badges.sales_orders = Some(BadgeCount { count });
    }
    if wants(BadgePart::SupportUnread) {
        let count = crate::routes::support_chat::client_unread(&state.pool, user.id).await?;
        badges.support_unread = Some(BadgeCount { count });
    }
    Ok(Json(badges))
}
