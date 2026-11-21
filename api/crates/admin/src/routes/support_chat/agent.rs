//! The support team's side of the support chat (the Support app).

use super::{
    CATEGORIES, Choice, MESSAGE_COLUMNS, MessageRow, POSTHOG_PROJECT_URL, PRIORITIES, STATUSES,
    SupportAgent, SupportMessage, choices, is_choice, message_body, naive_js, naive_js_opt,
};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::{IntoParams, ToSchema};
use crate::serde_helpers::double_option;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;

/// Conversations one inbox read returns at most.
const INBOX_LIMIT: i64 = 200;
/// Messages one read of a conversation returns at most.
const MESSAGES_LIMIT: i64 = 500;

/// A member of the support team.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct Agent {
    /// User id.
    pub id: i32,
    /// Name; null when the user has none.
    pub name: Option<String>,
    /// Login email.
    pub email: String,
}

/// `GET /v1/support/agent/bootstrap`.
#[derive(Debug, Serialize, ToSchema)]
pub struct Bootstrap {
    /// The signed-in team member.
    pub me: Agent,
    /// Everyone a request can be assigned to: every superuser.
    pub agents: Vec<Agent>,
    /// Request types (`category`).
    pub categories: Vec<Choice>,
    /// Statuses.
    pub statuses: Vec<Choice>,
    /// Priorities, lowest first.
    pub priorities: Vec<Choice>,
    /// The PostHog project the CRM records to.
    pub posthog_project_url: &'static str,
}

async fn superusers(pool: &MySqlPool) -> sqlx::Result<Vec<Agent>> {
    sqlx::query_as(
        "SELECT id, name, TRIM(email) AS email FROM users
          WHERE is_superuser = 1 AND is_deleted = 0 ORDER BY name, id",
    )
    .fetch_all(pool)
    .await
}

/// Start the Support app
///
/// Who is signed in, the team members requests can be assigned to (every
/// superuser) and the choices of the request fields: types, statuses,
/// priorities. Superusers whose session began with two-step sign-in only
/// (403 otherwise: the app then asks to sign in again). Next:
/// `GET /v1/support/agent/conversations`.
#[crate::openapi::path(
    get,
    path = "/v1/support/agent/bootstrap",
    operation_id = "start_support_app",
    tag = "support",
    responses(
        (status = 200, description = "The signed-in member, the team and the choices", body = Bootstrap),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not a superuser, or not signed in with two-step sign-in", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn bootstrap(
    State(state): State<AppState>,
    SupportAgent(user): SupportAgent,
) -> ApiResult<Json<Bootstrap>> {
    Ok(Json(Bootstrap {
        me: Agent {
            id: user.id,
            name: user.name,
            email: user.email,
        },
        agents: superusers(&state.pool).await?,
        categories: choices(&CATEGORIES),
        statuses: choices(&STATUSES),
        priorities: choices(&PRIORITIES),
        posthog_project_url: POSTHOG_PROJECT_URL,
    }))
}

/// A support request as the inbox lists it.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct ConversationSummary {
    /// Conversation id.
    pub id: i32,
    /// `open` (the team owes an answer), `waiting` (on the client), `resolved`.
    pub status: String,
    /// Request type (see `categories` of the bootstrap); null until set.
    pub category: Option<String>,
    /// `low`, `normal`, `high` or `urgent`.
    pub priority: String,
    /// The team member handling it; null when unassigned.
    pub assignee_id: Option<i32>,
    /// Their name.
    pub assignee_name: Option<String>,
    /// The first line of the first message.
    pub subject: String,
    /// The start of the latest message.
    pub last_message: Option<String>,
    /// The latest message is the team's (an answer or a note).
    pub last_from_support: bool,
    /// When the latest message was written.
    #[serde(serialize_with = "naive_js")]
    #[schema(value_type = String, format = DateTime)]
    pub last_message_at: NaiveDateTime,
    /// When the request began.
    #[serde(serialize_with = "naive_js")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: NaiveDateTime,
    /// The client wrote since the team last read it.
    pub unread: bool,
    /// The client (user id).
    pub user_id: i32,
    /// The client's name.
    pub user_name: Option<String>,
    /// The client's email.
    pub user_email: String,
    /// The client's company.
    pub company_id: i32,
    /// Its name.
    pub company_name: String,
}

const SUMMARY_SELECT: &str = "SELECT c.id, c.status, c.category, c.priority, c.assignee_id,
       a.name AS assignee_name, c.subject, LEFT(m.body, 160) AS last_message,
       COALESCE(m.from_support, 0) = 1 AS last_from_support, c.last_message_at, c.created_at,
       (c.last_client_message_at IS NOT NULL
        AND c.last_client_message_at > COALESCE(c.agent_read_at, '1000-01-01')) AS unread,
       c.user_id, u.name AS user_name, TRIM(u.email) AS user_email,
       c.company_id, co.name AS company_name
  FROM support_conversations c
  JOIN users u ON u.id = c.user_id
  JOIN company co ON co.id = c.company_id
  LEFT JOIN users a ON a.id = c.assignee_id
  LEFT JOIN support_messages m ON m.id = c.last_message_id";

/// Which requests the inbox shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InboxView {
    /// Open and waiting (default).
    #[default]
    Active,
    /// The team owes an answer.
    Open,
    /// Waiting on the client.
    Waiting,
    /// Done.
    Resolved,
    /// Everything.
    All,
}

/// Query of `GET /v1/support/agent/conversations`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct InboxQuery {
    /// Which statuses (default `active`: open and waiting).
    #[serde(default)]
    #[param(inline)]
    pub view: InboxView,
    /// `me` (assigned to the caller), `unassigned`, or a user id; leave out
    /// for everyone's.
    pub assignee: Option<String>,
    /// Words to find in the subject, the client's name or email, or the
    /// company's name.
    pub q: Option<String>,
    /// The `version` of the caller's last answer: when nothing changed
    /// since, the answer is `{"unchanged": true}` without the lists.
    pub version: Option<String>,
}

/// Requests per pile, for the app's sidebar (resolved ones not counted).
#[derive(Debug, Default, Serialize, sqlx::FromRow, ToSchema)]
pub struct InboxCounts {
    /// Open and waiting.
    pub active: i64,
    /// The team owes an answer.
    pub open: i64,
    /// Waiting on the client.
    pub waiting: i64,
    /// Active ones nobody is assigned to.
    pub unassigned: i64,
    /// Active ones assigned to the caller.
    pub mine: i64,
    /// Active ones the client wrote in since the team last read them.
    pub unread: i64,
}

/// `GET /v1/support/agent/conversations`.
#[derive(Debug, Serialize, ToSchema)]
pub struct Inbox {
    /// Nothing changed since the `version` asked with: the other fields are
    /// left out.
    pub unchanged: bool,
    /// Send back as `version` on the next poll.
    pub version: String,
    /// Counts per pile.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counts: Option<InboxCounts>,
    /// The requests, unread first, then the latest message first (at most
    /// 200).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversations: Option<Vec<ConversationSummary>>,
}

/// What changed last in any request, and how many there are.
async fn inbox_version(pool: &MySqlPool) -> sqlx::Result<String> {
    let (latest, count): (Option<String>, i64) =
        sqlx::query_as("SELECT CAST(MAX(updated_at) AS CHAR), COUNT(*) FROM support_conversations")
            .fetch_one(pool)
            .await?;
    Ok(format!("{}|{count}", latest.unwrap_or_default()))
}

/// List support requests
///
/// The Support app's inbox: support requests, unread ones first, then the
/// latest message first (at most 200), with the client, their company, the
/// assignee and the start of the latest message, plus counts per pile.
/// `view` picks the statuses (default `active`: open and waiting),
/// `assignee` whose (`me`, `unassigned`, a user id), `q` searches the
/// subject, the client and the company. Poll it with `version` (from the
/// last answer): when no request changed, it answers `unchanged` only.
/// Open one with `GET /v1/support/agent/conversations/{id}`.
#[crate::openapi::path(
    get,
    path = "/v1/support/agent/conversations",
    operation_id = "list_support_conversations",
    tag = "support",
    params(InboxQuery),
    responses(
        (status = 200, description = "The inbox, or `unchanged`", body = Inbox),
        (status = 400, description = "Unknown assignee", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not a superuser, or not signed in with two-step sign-in", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_conversations(
    State(state): State<AppState>,
    SupportAgent(user): SupportAgent,
    Query(query): Query<InboxQuery>,
) -> ApiResult<Json<Inbox>> {
    let pool = &state.pool;
    let version = inbox_version(pool).await?;
    if query.version.as_deref() == Some(version.as_str()) {
        return Ok(Json(Inbox {
            unchanged: true,
            version,
            counts: None,
            conversations: None,
        }));
    }
    let counts: InboxCounts = sqlx::query_as(
        "SELECT CAST(COUNT(*) AS SIGNED) AS active,
                CAST(COALESCE(SUM(status = 'open'), 0) AS SIGNED) AS open,
                CAST(COALESCE(SUM(status = 'waiting'), 0) AS SIGNED) AS waiting,
                CAST(COALESCE(SUM(assignee_id IS NULL), 0) AS SIGNED) AS unassigned,
                CAST(COALESCE(SUM(assignee_id = ?), 0) AS SIGNED) AS mine,
                CAST(COALESCE(SUM(last_client_message_at IS NOT NULL
                    AND last_client_message_at > COALESCE(agent_read_at, '1000-01-01')), 0) AS SIGNED) AS unread
           FROM support_conversations WHERE status IN ('open', 'waiting')",
    )
    .bind(user.id)
    .fetch_one(pool)
    .await?;

    let mut sql = format!("{SUMMARY_SELECT} WHERE 1 = 1");
    let mut binds: Vec<Bind> = Vec::new();
    match query.view {
        InboxView::Active => sql.push_str(" AND c.status IN ('open', 'waiting')"),
        InboxView::Open => sql.push_str(" AND c.status = 'open'"),
        InboxView::Waiting => sql.push_str(" AND c.status = 'waiting'"),
        InboxView::Resolved => sql.push_str(" AND c.status = 'resolved'"),
        InboxView::All => {}
    }
    match query.assignee.as_deref().map(str::trim) {
        None | Some("" | "any") => {}
        Some("me") => {
            sql.push_str(" AND c.assignee_id = ?");
            binds.push(Bind::Int(user.id));
        }
        Some("unassigned") => sql.push_str(" AND c.assignee_id IS NULL"),
        Some(id) => {
            let id: i32 = id.parse().map_err(|_| {
                ApiError::BadRequest("assignee is me, unassigned or a user id".into())
            })?;
            sql.push_str(" AND c.assignee_id = ?");
            binds.push(Bind::Int(id));
        }
    }
    if let Some(q) = query.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        let like = format!(
            "%{}%",
            q.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        sql.push_str(
            " AND (c.subject LIKE ? OR u.name LIKE ? OR u.email LIKE ? OR co.name LIKE ?)",
        );
        for _ in 0..4 {
            binds.push(Bind::Text(like.clone()));
        }
    }
    sql.push_str(" ORDER BY unread DESC, c.last_message_at DESC, c.id DESC LIMIT ");
    sql.push_str(&INBOX_LIMIT.to_string());
    let mut select = sqlx::query_as::<_, ConversationSummary>(sqlx::AssertSqlSafe(sql));
    for bind in binds {
        select = match bind {
            Bind::Int(value) => select.bind(value),
            Bind::Text(value) => select.bind(value),
        };
    }
    let conversations = select.fetch_all(pool).await?;
    Ok(Json(Inbox {
        unchanged: false,
        version,
        counts: Some(counts),
        conversations: Some(conversations),
    }))
}

enum Bind {
    Int(i32),
    Text(String),
}

async fn summary(pool: &MySqlPool, id: i32) -> ApiResult<ConversationSummary> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SUMMARY_SELECT} WHERE c.id = ?"
    )))
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or(ApiError::NotFound)
}

/// Who wrote, for the team: what helps answer without asking.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct ClientInfo {
    /// User id (also their PostHog person id).
    pub user_id: i32,
    /// Name.
    pub name: Option<String>,
    /// Login email.
    pub email: String,
    /// Personal phone number.
    pub phone_number: Option<String>,
    /// Company admin.
    pub is_admin: bool,
    /// Staff member of the company.
    pub is_employee: bool,
    /// Their positions in the company, comma separated (`sales_rep, shop_worker`).
    pub positions: Option<String>,
    /// When the account was made.
    #[serde(serialize_with = "naive_js_opt")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub user_created_at: Option<NaiveDateTime>,
    /// Their latest sign-in on the website.
    #[serde(serialize_with = "naive_js_opt")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub last_sign_in_at: Option<NaiveDateTime>,
    /// They use the phone app (a phone signed in).
    pub uses_phone_app: bool,
    /// Their company.
    pub company_id: i32,
    /// Its name.
    pub company_name: String,
    /// Its web address label (`acme` of acme.granite-manager.com); null when none.
    pub subdomain: Option<String>,
    /// `superuser` (set up by Granite Manager) or `signup_page` (signed itself up).
    pub source: String,
    /// When it signed itself up; null for companies Granite Manager set up.
    #[serde(serialize_with = "naive_js_opt")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub signed_up_at: Option<NaiveDateTime>,
    /// Free access granted until; null when none.
    #[serde(serialize_with = "naive_js_opt")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub free_access_until: Option<NaiveDateTime>,
    /// Live users of the company.
    pub user_count: i64,
    /// Its user limit.
    pub max_users: Option<i32>,
    /// Its customers (leads included).
    pub customer_count: i64,
    /// Stripe subscription status (`active`, `past_due`, `canceled`, `none`, ...); null when never billed.
    pub subscription_status: Option<String>,
    /// Its plan name.
    pub plan_name: Option<String>,
    /// What it pays a month, in cents.
    pub monthly_price_cents: Option<i32>,
    /// When the paid period ends.
    #[serde(serialize_with = "naive_js_opt")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub current_period_end: Option<NaiveDateTime>,
}

async fn client_info(pool: &MySqlPool, user_id: i32, company_id: i32) -> sqlx::Result<ClientInfo> {
    sqlx::query_as(
        "SELECT u.id AS user_id, u.name, TRIM(u.email) AS email, u.phone_number,
                COALESCE(u.is_admin, 0) = 1 AS is_admin, COALESCE(u.is_employee, 0) = 1 AS is_employee,
                (SELECT GROUP_CONCAT(DISTINCT p.name ORDER BY p.name SEPARATOR ', ')
                   FROM users_positions up JOIN positions p ON p.id = up.position_id
                  WHERE up.user_id = u.id AND up.company_id = co.id) AS positions,
                CAST(u.created_date AS DATETIME) AS user_created_at,
                (SELECT CAST(MAX(s.created_date) AS DATETIME) FROM sessions s WHERE s.user_id = u.id)
                  AS last_sign_in_at,
                EXISTS (SELECT 1 FROM device_sessions d WHERE d.user_id = u.id) AS uses_phone_app,
                co.id AS company_id, co.name AS company_name, co.subdomain, co.source,
                co.signed_up_at, co.free_access_until,
                (SELECT COUNT(*) FROM users cu WHERE cu.company_id = co.id AND cu.is_deleted = 0)
                  AS user_count,
                co.max_users,
                (SELECT COUNT(*) FROM customers cs WHERE cs.company_id = co.id AND cs.deleted_at IS NULL)
                  AS customer_count,
                b.subscription_status, b.plan_name, b.monthly_price_cents, b.current_period_end
           FROM users u
           JOIN company co ON co.id = ?
           LEFT JOIN company_billing b ON b.company_id = co.id
          WHERE u.id = ?",
    )
    .bind(company_id)
    .bind(user_id)
    .fetch_one(pool)
    .await
}

/// Query of `GET /v1/support/agent/conversations/{id}`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ConversationQuery {
    /// Only messages newer than this id (a poll of an open conversation):
    /// the client and their other requests are then left out.
    pub after: Option<i64>,
}

/// `GET /v1/support/agent/conversations/{id}`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConversationDetail {
    /// The request.
    pub conversation: ConversationSummary,
    /// Its messages, oldest first, internal notes included, each with the
    /// page the client was on and the link to their PostHog recording.
    pub messages: Vec<SupportMessage>,
    /// Who wrote; left out with `after`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<ClientInfo>,
    /// The client's PostHog person page (every recording of theirs); left out with `after`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub posthog_person_url: Option<String>,
    /// The company's other requests, latest first (at most 20); left out with `after`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<ConversationSummary>>,
}

/// Open a support request
///
/// One request with its messages (oldest first, the team's internal notes
/// included, each client message with the page they were on, their browser
/// and the link to their PostHog recording at that moment), who wrote
/// (their account, company, plan and usage), their PostHog person page and
/// the company's other requests. Poll an open one with `after` (the newest
/// message id): only newer messages and the request's state come back.
/// Mark it read with `POST .../read`; answer with `POST .../messages`.
#[crate::openapi::path(
    get,
    path = "/v1/support/agent/conversations/{id}",
    operation_id = "get_support_conversation",
    tag = "support",
    params(("id" = i32, Path, description = "Conversation id"), ConversationQuery),
    responses(
        (status = 200, description = "The request", body = ConversationDetail),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not a superuser, or not signed in with two-step sign-in", body = ErrorBody),
        (status = 404, description = "No such request", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_conversation(
    State(state): State<AppState>,
    _agent: SupportAgent,
    Path(id): Path<i32>,
    Query(query): Query<ConversationQuery>,
) -> ApiResult<Json<ConversationDetail>> {
    let pool = &state.pool;
    let conversation = summary(pool, id).await?;
    let rows: Vec<MessageRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {MESSAGE_COLUMNS}
           FROM support_messages m
           LEFT JOIN users au ON au.id = m.author_id
          WHERE m.conversation_id = ? AND m.id > ?
          ORDER BY m.id LIMIT {MESSAGES_LIMIT}"
    )))
    .bind(id)
    .bind(query.after.unwrap_or(0))
    .fetch_all(pool)
    .await?;
    let messages = rows.into_iter().map(MessageRow::for_team).collect();
    if query.after.is_some() {
        return Ok(Json(ConversationDetail {
            conversation,
            messages,
            client: None,
            posthog_person_url: None,
            history: None,
        }));
    }
    let client = client_info(pool, conversation.user_id, conversation.company_id).await?;
    let history: Vec<ConversationSummary> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SUMMARY_SELECT} WHERE c.company_id = ? AND c.id <> ? ORDER BY c.id DESC LIMIT 20"
    )))
    .bind(conversation.company_id)
    .bind(id)
    .fetch_all(pool)
    .await?;
    Ok(Json(ConversationDetail {
        posthog_person_url: Some(format!(
            "{POSTHOG_PROJECT_URL}/person/{}",
            conversation.user_id
        )),
        conversation,
        messages,
        client: Some(client),
        history: Some(history),
    }))
}

/// Body of `PATCH /v1/support/agent/conversations/{id}`; fields left out
/// stay as they are.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ConversationChange {
    /// A new subject (what the request is about; at most 200 characters).
    #[serde(default)]
    pub subject: Option<String>,
    /// `open`, `waiting` or `resolved`.
    #[serde(default)]
    pub status: Option<String>,
    /// A request type (see the bootstrap's `categories`), or null to clear.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<String>)]
    pub category: Option<Option<String>>,
    /// `low`, `normal`, `high` or `urgent`.
    #[serde(default)]
    pub priority: Option<String>,
    /// A superuser's id, or null to unassign.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<i32>)]
    pub assignee_id: Option<Option<i32>>,
}

/// Change a support request
///
/// Sets its subject, status (`resolved` closes it; the client writing in it
/// opens it again), type, priority or assignee (any superuser, or null to
/// unassign). Fields left out stay. Returns the request as the inbox lists
/// it.
#[crate::openapi::path(
    patch,
    path = "/v1/support/agent/conversations/{id}",
    operation_id = "update_support_conversation",
    tag = "support",
    params(("id" = i32, Path, description = "Conversation id")),
    request_body = ConversationChange,
    responses(
        (status = 200, description = "Changed", body = ConversationSummary),
        (status = 400, description = "Empty or long subject, unknown status, type or priority, or the assignee is not a superuser", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not a superuser, or not signed in with two-step sign-in", body = ErrorBody),
        (status = 404, description = "No such request", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_conversation(
    State(state): State<AppState>,
    _agent: SupportAgent,
    Path(id): Path<i32>,
    JsonBody(change): JsonBody<ConversationChange>,
) -> ApiResult<Json<ConversationSummary>> {
    let pool = &state.pool;
    let current = summary(pool, id).await?;
    if let Some(status) = change.status.as_deref()
        && !is_choice(&STATUSES, status)
    {
        return Err(ApiError::BadRequest(
            "status is open, waiting or resolved".into(),
        ));
    }
    if let Some(Some(category)) = change.category.as_ref()
        && !is_choice(&CATEGORIES, category)
    {
        return Err(ApiError::BadRequest("Unknown request type".into()));
    }
    if let Some(priority) = change.priority.as_deref()
        && !is_choice(&PRIORITIES, priority)
    {
        return Err(ApiError::BadRequest(
            "priority is low, normal, high or urgent".into(),
        ));
    }
    if let Some(Some(assignee)) = change.assignee_id {
        let superuser: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM users WHERE id = ? AND is_superuser = 1 AND is_deleted = 0",
        )
        .bind(assignee)
        .fetch_optional(pool)
        .await?;
        if superuser.is_none() {
            return Err(ApiError::BadRequest("Assign a superuser".into()));
        }
    }
    let subject = match change.subject.as_deref().map(str::trim) {
        None => current.subject,
        Some("") => return Err(ApiError::BadRequest("Write a subject".into())),
        Some(s) if s.chars().count() > 200 => {
            return Err(ApiError::BadRequest(
                "A subject is at most 200 characters".into(),
            ));
        }
        Some(s) => s.to_string(),
    };
    let status = change.status.unwrap_or(current.status);
    let category = change.category.unwrap_or(current.category);
    let priority = change.priority.unwrap_or(current.priority);
    let assignee = change.assignee_id.unwrap_or(current.assignee_id);
    sqlx::query(
        "UPDATE support_conversations
            SET subject = ?, status = ?, category = ?, priority = ?, assignee_id = ?,
                resolved_at = CASE WHEN ? = 'resolved' THEN COALESCE(resolved_at, NOW(3)) END
          WHERE id = ?",
    )
    .bind(subject)
    .bind(&status)
    .bind(category)
    .bind(priority)
    .bind(assignee)
    .bind(&status)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(Json(summary(pool, id).await?))
}

/// Body of `POST /v1/support/agent/conversations/{id}/messages`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentReply {
    /// The text (at most 5000 characters).
    pub body: String,
    /// An internal note for the team: the client never sees it, and the
    /// request's status stays.
    #[serde(default)]
    pub internal: bool,
}

/// Answer a support request
///
/// Sends the team's answer to the client (it shows in their support chat
/// and badges the Chat button), or with `internal` adds a note only the
/// team sees. An answer makes the request `waiting` (on the client), and
/// assigns it to the one answering when nobody has it. Marks the request
/// read. Returns the saved message.
#[crate::openapi::path(
    post,
    path = "/v1/support/agent/conversations/{id}/messages",
    operation_id = "answer_support_conversation",
    tag = "support",
    params(("id" = i32, Path, description = "Conversation id")),
    request_body = AgentReply,
    responses(
        (status = 201, description = "Sent", body = SupportMessage),
        (status = 400, description = "Empty or too long", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not a superuser, or not signed in with two-step sign-in", body = ErrorBody),
        (status = 404, description = "No such request", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn reply(
    State(state): State<AppState>,
    SupportAgent(user): SupportAgent,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<AgentReply>,
) -> ApiResult<(StatusCode, Json<SupportMessage>)> {
    let body = message_body(&input.body)?;
    let mut tx = state.pool.begin().await?;
    let exists: Option<i32> =
        sqlx::query_scalar("SELECT id FROM support_conversations WHERE id = ? FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    if exists.is_none() {
        return Err(ApiError::NotFound);
    }
    let message_id = sqlx::query(
        "INSERT INTO support_messages (conversation_id, author_id, from_support, internal, body)
         VALUES (?, ?, 1, ?, ?)",
    )
    .bind(id)
    .bind(user.id)
    .bind(input.internal)
    .bind(&body)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    let message_id =
        i64::try_from(message_id).map_err(|err| ApiError::Internal(err.to_string()))?;
    if input.internal {
        sqlx::query(
            "UPDATE support_conversations
                SET last_message_id = ?, last_message_at = NOW(3), agent_read_at = NOW(3)
              WHERE id = ?",
        )
        .bind(message_id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query(
            "UPDATE support_conversations
                SET status = 'waiting', resolved_at = NULL, last_message_id = ?,
                    last_message_at = NOW(3), last_agent_message_at = NOW(3),
                    agent_read_at = NOW(3), assignee_id = COALESCE(assignee_id, ?)
              WHERE id = ?",
        )
        .bind(message_id)
        .bind(user.id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    let row: MessageRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {MESSAGE_COLUMNS} FROM support_messages m
           LEFT JOIN users au ON au.id = m.author_id WHERE m.id = ?"
    )))
    .bind(message_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(row.for_team())))
}

/// Mark a support request read
///
/// The team has read the client's messages: the request loses its unread
/// mark in every member's inbox. Call it when the request is on screen and
/// the client wrote since.
#[crate::openapi::path(
    post,
    path = "/v1/support/agent/conversations/{id}/read",
    operation_id = "mark_support_conversation_read",
    tag = "support",
    params(("id" = i32, Path, description = "Conversation id")),
    responses(
        (status = 204, description = "Marked read"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not a superuser, or not signed in with two-step sign-in", body = ErrorBody),
        (status = 404, description = "No such request", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn mark_read(
    State(state): State<AppState>,
    _agent: SupportAgent,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    let found: Option<i32> =
        sqlx::query_scalar("SELECT id FROM support_conversations WHERE id = ?")
            .bind(id)
            .fetch_optional(&state.pool)
            .await?;
    if found.is_none() {
        return Err(ApiError::NotFound);
    }
    sqlx::query(
        "UPDATE support_conversations SET agent_read_at = NOW(3)
          WHERE id = ? AND last_client_message_at > COALESCE(agent_read_at, '1000-01-01')",
    )
    .bind(id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
