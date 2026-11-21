//! Support chat: CRM users write to Granite Manager's support team from the
//! Chat panel ("Support"), and the team answers from the Support app
//! (`support.granite-manager.com`, `apps/support` in granite-front).
//!
//! The client's side (any signed-in user, their own conversations only):
//!
//! - `GET  /v1/support/conversations`       → their conversations (separate requests), by subject
//! - `GET  /v1/support/conversations/{id}`  → one conversation's messages
//! - `POST /v1/support/conversations/{id}/read` → mark its answers read
//! - `GET  /v1/support/thread`          → every message of theirs at once (older clients)
//! - `POST /v1/support/messages`        → write in a conversation, or start a new one
//! - `POST /v1/support/thread/read`     → mark the team's answers read
//!
//! The team's side (superusers signed in with two-step sign-in, by session
//! cookie only; see [`SupportAgent`]):
//!
//! - `GET   /v1/support/agent/bootstrap`                    → who is signed in, the team, the choices
//! - `GET   /v1/support/agent/conversations`                → the inbox (with `version`, unchanged answers are tiny)
//! - `GET   /v1/support/agent/conversations/{id}`           → a request: messages, the client, their other requests
//! - `PATCH /v1/support/agent/conversations/{id}`           → status, type, priority, assignee
//! - `POST  /v1/support/agent/conversations/{id}/messages`  → answer, or an internal note
//! - `POST  /v1/support/agent/conversations/{id}/read`      → mark the client's messages read
//!
//! No websockets: both sides poll, and only while it matters (the client
//! while the Support chat is on screen, plus the minute badge poll; the app
//! while its tab is open, slower when hidden). Every poll is one or two
//! indexed reads.

pub mod agent;
pub mod client;
pub mod conversations;

use crate::auth::CurrentUser;
use crate::auth::cookie::request_session_id;
use crate::auth::session::{SessionUser, session_key};
use crate::error::ApiError;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;
use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use chrono::NaiveDateTime;
use serde::{Serialize, Serializer};

pub use client::client_unread;

/// How long the subject model may take before the first line stays.
const SUBJECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

const SUBJECT_PROMPT: &str = "Write the subject of this support request from a user of Granite Manager, a CRM for countertop and stone fabricators. 3 to 7 words, sentence case, in the language of the message, no quotes, no period at the end. Answer with the subject only.";

/// The model's answer as a subject: one line, no quotes, at most 120 characters.
pub fn clean_subject(raw: &str) -> Option<String> {
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    let line = line
        .trim_start_matches(['"', '\'', '“', '”', '«', '»'])
        .trim_end_matches(['"', '\'', '“', '”', '«', '»', '.'])
        .trim();
    let line = line.strip_prefix("Subject:").map_or(line, str::trim);
    (!line.is_empty()).then(|| line.chars().take(120).collect())
}

/// A short subject for a new request, written by the cheap chat model
/// (Luna); `None` when the model is off, slow or fails (the first line stays).
pub async fn ai_subject(state: &AppState, user: &SessionUser, text: &str) -> Option<String> {
    use crate::ai::{Caller, Chat, MINI_FALLBACK_MODEL, MINI_MODEL};
    let text: String = text.chars().take(2000).collect();
    let chat = |model| {
        let mut chat = Chat::new(model)
            .max_tokens(Some(300))
            .message("system", SUBJECT_PROMPT)
            .message("user", &text);
        chat.reasoning_effort = Some("low");
        chat.timeout = SUBJECT_TIMEOUT;
        chat
    };
    let caller = Caller {
        feature: "support_subject",
        user_id: Some(user.id),
        company_id: Some(user.company_id),
    };
    let reply = match state.ai.chat(&state.pool, caller, &chat(MINI_MODEL)).await {
        Err(err) if err.is_model_permission() => {
            state
                .ai
                .chat(&state.pool, caller, &chat(MINI_FALLBACK_MODEL))
                .await
        }
        reply => reply,
    };
    reply.ok().as_deref().and_then(clean_subject)
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(client::get_thread))
        .routes(routes!(client::send_message))
        .routes(routes!(client::mark_thread_read))
        .routes(routes!(conversations::list_mine))
        .routes(routes!(conversations::get_mine))
        .routes(routes!(conversations::read_mine))
        .routes(routes!(agent::bootstrap))
        .routes(routes!(agent::list_conversations))
        .routes(routes!(agent::get_conversation, agent::update_conversation))
        .routes(routes!(agent::reply))
        .routes(routes!(agent::mark_read))
}

/// The request types the team picks from: `(value, label)`.
pub const CATEGORIES: [(&str, &str); 8] = [
    ("how_to", "How-to / training"),
    ("bug", "Bug"),
    ("feature_request", "Feature request"),
    ("billing", "Billing"),
    ("account_access", "Login & access"),
    ("data_request", "Data change / import"),
    ("integration", "Integrations"),
    ("other", "Other"),
];
/// Statuses: the team owes an answer, the client does, done.
pub const STATUSES: [(&str, &str); 3] = [
    ("open", "Open"),
    ("waiting", "Waiting on client"),
    ("resolved", "Resolved"),
];
/// Priorities, lowest first.
pub const PRIORITIES: [(&str, &str); 4] = [
    ("low", "Low"),
    ("normal", "Normal"),
    ("high", "High"),
    ("urgent", "Urgent"),
];

/// Longest message, in characters.
pub const BODY_MAX: usize = 5000;
/// Pictures one message may carry.
pub const MAX_IMAGES: usize = 4;
/// The PostHog project the CRM records to: replays and people are linked here.
pub const POSTHOG_PROJECT_URL: &str = "https://us.posthog.com/project/141685";

/// A value with its label, for a select.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Choice {
    /// The value to send.
    pub value: &'static str,
    /// What to show.
    pub label: &'static str,
}

pub fn choices(list: &[(&'static str, &'static str)]) -> Vec<Choice> {
    list.iter()
        .map(|&(value, label)| Choice { value, label })
        .collect()
}

pub fn is_choice(list: &[(&str, &str)], value: &str) -> bool {
    list.iter().any(|(known, _)| *known == value)
}

/// One message of a support conversation.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SupportMessage {
    /// Message id; ids grow, so the newest one is the `after` of the next poll.
    pub id: i64,
    /// The conversation (support request) it belongs to.
    pub conversation_id: i32,
    /// Written by the support team (else by the client).
    pub from_support: bool,
    /// An internal note of the team; the client never sees these.
    pub internal: bool,
    /// The text, as written.
    pub body: String,
    /// Who wrote it (the team member's or the client's name); null when unknown.
    pub author_name: Option<String>,
    /// Pictures added to it (public storage URLs).
    pub images: Vec<String>,
    /// When it was written (ISO 8601, UTC).
    #[serde(serialize_with = "naive_js")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: NaiveDateTime,
    /// The team's view only: the CRM page the client was on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_path: Option<String>,
    /// The team's view only: the PostHog recording of the client's
    /// session, at the moment they wrote.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replay_url: Option<String>,
    /// The team's view only: the client's browser.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// The team's view only: the client's window size (`1440x900`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen: Option<String>,
}

/// A message as stored.
#[derive(Debug, sqlx::FromRow)]
pub struct MessageRow {
    pub id: i64,
    pub conversation_id: i32,
    pub from_support: bool,
    pub internal: bool,
    pub body: String,
    pub author_name: Option<String>,
    pub image_urls: Option<String>,
    pub page_path: Option<String>,
    pub posthog_session_id: Option<String>,
    pub replay_seconds: Option<i32>,
    pub user_agent: Option<String>,
    pub screen: Option<String>,
    pub created_at: NaiveDateTime,
}

/// The columns of [`MessageRow`] (messages `m`, authors `au`).
pub const MESSAGE_COLUMNS: &str = "m.id, m.conversation_id, m.from_support, m.internal, m.body,
       au.name AS author_name, m.image_urls, m.page_path, m.posthog_session_id,
       m.replay_seconds, m.user_agent, m.screen, m.created_at";

/// The PostHog recording of a session, `seconds` in.
pub fn replay_url(session_id: &str, seconds: Option<i32>) -> String {
    match seconds {
        Some(seconds) if seconds > 0 => {
            format!("{POSTHOG_PROJECT_URL}/replay/{session_id}?t={seconds}")
        }
        _ => format!("{POSTHOG_PROJECT_URL}/replay/{session_id}"),
    }
}

impl MessageRow {
    /// The message as the team sees it: with where the client was.
    pub fn for_team(self) -> SupportMessage {
        let replay = self
            .posthog_session_id
            .as_deref()
            .map(|id| replay_url(id, self.replay_seconds));
        SupportMessage {
            replay_url: replay,
            page_path: self.page_path.clone(),
            user_agent: self.user_agent.clone(),
            screen: self.screen.clone(),
            ..self.for_client()
        }
    }

    /// The message as the client sees it.
    pub fn for_client(self) -> SupportMessage {
        SupportMessage {
            id: self.id,
            conversation_id: self.conversation_id,
            from_support: self.from_support,
            internal: self.internal,
            images: self
                .image_urls
                .as_deref()
                .and_then(|json| serde_json::from_str(json).ok())
                .unwrap_or_default(),
            body: self.body,
            author_name: self.author_name,
            created_at: self.created_at,
            page_path: None,
            replay_url: None,
            user_agent: None,
            screen: None,
        }
    }
}

/// Serializes a UTC `DATETIME` like JavaScript's `Date.toJSON`.
pub fn naive_js<S: Serializer>(value: &NaiveDateTime, serializer: S) -> Result<S::Ok, S::Error> {
    crate::serde_helpers::js_date_required(&value.and_utc(), serializer)
}

/// [`naive_js`] for an optional date.
pub fn naive_js_opt<S: Serializer>(
    value: &Option<NaiveDateTime>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    crate::serde_helpers::js_date(&value.map(|v| v.and_utc()), serializer)
}

/// The message text: trimmed, not empty, at most [`BODY_MAX`] characters.
pub fn message_body(body: &str) -> Result<String, ApiError> {
    let body = body.trim();
    if body.is_empty() {
        return Err(ApiError::BadRequest("Write a message first.".into()));
    }
    if body.chars().count() > BODY_MAX {
        return Err(ApiError::BadRequest(format!(
            "A message is at most {BODY_MAX} characters."
        )));
    }
    Ok(body.to_string())
}

/// Said to someone who may not use the Support app.
pub const NOT_SUPPORT: &str = "The Support app is for Granite Manager's superusers.";
/// Said to a superuser without two-step sign-in.
pub const NEEDS_MFA: &str = "Sign in again with your authenticator code to use the Support app.";

/// A member of the support team, else 403.
///
/// A superuser signed in with the session cookie (not an agent or phone
/// token) whose session began with two-step sign-in (it was on when they
/// signed in).
pub struct SupportAgent(pub SessionUser);

impl FromRequestParts<AppState> for SupportAgent {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        // A token in `Authorization` is an agent's or the phone's, never the
        // team's browser session.
        if parts.headers.contains_key(AUTHORIZATION) {
            return Err(ApiError::Denied(NOT_SUPPORT.into()));
        }
        let CurrentUser(user) = CurrentUser::from_request_parts(parts, state).await?;
        if !user.is_superuser {
            return Err(ApiError::Denied(NOT_SUPPORT.into()));
        }
        // Two-step sign-in is on, and this session began after it was
        // turned on, so signing in asked for the authenticator code.
        let session_id = request_session_id(&parts.headers, &state.session_secret)
            .ok_or(ApiError::Unauthorized)?;
        let passed: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM user_mfa mf
               JOIN sessions s ON s.id = ? AND s.user_id = mf.user_id
              WHERE mf.user_id = ? AND mf.secret IS NOT NULL AND mf.enabled_at IS NOT NULL
                AND s.created_date >= mf.enabled_at",
        )
        .bind(session_key(&session_id))
        .bind(user.id)
        .fetch_optional(&state.pool)
        .await?;
        if passed.is_none() {
            return Err(ApiError::Denied(NEEDS_MFA.into()));
        }
        Ok(Self(user))
    }
}
