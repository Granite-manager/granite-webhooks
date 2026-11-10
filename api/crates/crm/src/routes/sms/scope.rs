//! Which threads, and which rows in them, a user may see.
//!
//! A text is the rep's by its user or by its number, either one, and a rep
//! sees only their texts, never another rep's in the same thread: those sent
//! by the user or tagged with an agent id they have or ever had there, those
//! texted to or from a number of theirs, now or ever, on any provider
//! (`user_sms_identities`), and untagged ones on a shared company line or on
//! no line. "Mine" is the threads holding such a text. "All" — admins only —
//! is the whole company, or one rep's "mine" when filtered by agent.
//! Company lines and the onboarding sample are never thread keys.
//!
//! Telnyx: the user's numbers are the ones they may call from (a shared line
//! is a shared inbox); the rep filter takes the rep's user id.

use super::provider::{Param, SAMPLE_PHONES, SmsProvider, marks, phone10};
use crate::auth::session::SessionUser;
use crate::openapi::ToSchema;
use serde::Deserialize;
use sqlx::MySqlPool;
use std::collections::BTreeSet;

/// Whose conversations to list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SmsScope {
    /// The caller's own conversations.
    #[default]
    Mine,
    /// The whole company (admins; anyone else gets `mine`).
    All,
}

impl SmsScope {
    /// `all` only for admins.
    #[must_use]
    pub const fn for_user(self, user: &SessionUser) -> Self {
        match self {
            Self::All if user.is_admin || user.is_superuser => Self::All,
            _ => Self::Mine,
        }
    }
}

/// A company user with `agent_id`, when the scope is `all`; anything else
/// filters nothing.
pub async fn filter_agent(
    pool: &MySqlPool,
    provider: SmsProvider,
    user: &SessionUser,
    scope: SmsScope,
    agent_id: Option<&str>,
) -> Result<Option<String>, sqlx::Error> {
    let wanted = agent_id.map(str::trim).unwrap_or_default();
    if scope != SmsScope::All || wanted.is_empty() {
        return Ok(None);
    }
    let Some(column) = provider.agent_column() else {
        // Telnyx: the rep's user id.
        let Ok(rep) = wanted.parse::<i32>() else {
            return Ok(None);
        };
        let found: Option<i32> = sqlx::query_scalar(
            "SELECT id FROM users WHERE id = ? AND company_id = ? AND is_deleted = 0",
        )
        .bind(rep)
        .bind(user.company_id)
        .fetch_optional(pool)
        .await?;
        return Ok(found.map(|id| id.to_string()));
    };
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT TRIM({column}) FROM users
          WHERE company_id = ? AND is_deleted = 0 AND TRIM(IFNULL({column}, '')) = ?
          LIMIT 1"
    )))
    .bind(user.company_id)
    .bind(wanted)
    .fetch_optional(pool)
    .await
}

/// The threads a user may see.
#[derive(Debug)]
pub struct Narrowing {
    /// Visible thread keys; `None` = the whole company.
    pub phones: Option<Vec<String>>,
    /// The rep whose rows are visible.
    pub own: Option<RepRows>,
    /// Company lines, never thread keys.
    pub company_lines: Vec<String>,
    /// One rep's inbox (unread first), not the company's.
    pub one_rep: bool,
    /// Only the texts on this line (last ten digits).
    pub line: Option<String>,
}

/// A rep's rows: tagged with an agent id of theirs or sent by them, texted
/// to or from a number of theirs, and untagged ones on a shared line or on
/// none. A text another rep sent, or one on another rep's number, is not.
#[derive(Debug, Clone)]
pub struct RepRows {
    agents: Vec<String>,
    sender_user_id: Option<i32>,
    /// The rep's numbers (last ten digits).
    lines: Vec<String>,
    /// Untagged rows on these lines, or on none, are visible too; `None`:
    /// no untagged rows (Telnyx, whose lines are all the company's).
    shared_lines: Option<Vec<String>>,
}

/// The line a row of `s` is on: the one it went to, or came from.
const ROW_LINE: &str = "(CASE WHEN s.direction = 'inbound' THEN s.recipient10 ELSE s.sender10 END)";

impl RepRows {
    fn new(ids: RepIds, shared_lines: Option<Vec<String>>) -> Option<Self> {
        (!ids.agents.is_empty() || ids.user_id.is_some() || !ids.lines.is_empty()).then_some(Self {
            agents: ids.agents,
            sender_user_id: ids.user_id,
            lines: ids.lines,
            shared_lines,
        })
    }

    /// The predicates over `s` for the rep's own rows and those on a number
    /// of theirs, ORed: a thread holding one is theirs.
    fn mine(&self) -> (Vec<String>, Vec<Param>) {
        let mut parts = Vec::new();
        let mut params: Vec<Param> = Vec::new();
        if !self.agents.is_empty() {
            parts.push(format!("s.agent IN ({})", marks(self.agents.len())));
            params.extend(self.agents.iter().map(Param::from));
        }
        if let Some(user_id) = self.sender_user_id {
            parts.push("s.sender_user_id = ?".to_string());
            params.push(user_id.into());
        }
        if !self.lines.is_empty() {
            let (on_lines, line_params) = on_lines(&self.lines);
            parts.push(on_lines);
            params.extend(line_params);
        }
        (parts, params)
    }

    /// Every row of the rep's threads they see, as one predicate over `s`.
    fn visible(&self) -> (String, Vec<Param>) {
        let (mut parts, mut params) = self.mine();
        if let Some(shared) = &self.shared_lines {
            let mut untagged = format!("{ROW_LINE} IS NULL");
            if !shared.is_empty() {
                untagged = format!("{untagged} OR {ROW_LINE} IN ({})", marks(shared.len()));
                params.extend(shared.iter().map(Param::from));
            }
            parts.push(format!(
                "(s.agent IS NULL AND s.sender_user_id IS NULL AND ({untagged}))"
            ));
        }
        (format!("({})", parts.join(" OR ")), params)
    }
}

/// A rep's ids on `CloudTalk` or `RingCentral`: every agent id they have or
/// had there, and every number of theirs they have or had on any provider (a
/// number moved between providers stays theirs). Company lines are shared,
/// never one rep's.
struct RepIds {
    user_id: Option<i32>,
    agents: Vec<String>,
    lines: Vec<String>,
}

/// The caller's own agent ids and numbers on `provider` (see [`RepIds`]),
/// for the calls that follow the same user-or-number rule as texts.
pub async fn own_ids(
    pool: &MySqlPool,
    provider: SmsProvider,
    user: &SessionUser,
) -> Result<(Vec<String>, Vec<String>), sqlx::Error> {
    let company_lines = company_lines(pool, provider, user.company_id).await?;
    let ids = rep_ids(pool, provider, user, SmsScope::Mine, None, &company_lines).await?;
    Ok((ids.agents, ids.lines))
}

/// The caller's ids, or with `scope=all` those of the rep whose agent id is
/// `filter_agent`.
async fn rep_ids(
    pool: &MySqlPool,
    provider: SmsProvider,
    user: &SessionUser,
    scope: SmsScope,
    filter_agent: Option<&str>,
    company_lines: &[String],
) -> Result<RepIds, sqlx::Error> {
    let (user_id, agent, line) = if scope == SmsScope::All {
        let (Some(agent), Some(agent_column), Some(phone_column)) = (
            filter_agent,
            provider.agent_column(),
            provider.phone_column(),
        ) else {
            return Ok(RepIds {
                user_id: None,
                agents: Vec::new(),
                lines: Vec::new(),
            });
        };
        let rep: Option<(i32, Option<String>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT id, {phone_column} FROM users
              WHERE company_id = ? AND is_deleted = 0 AND TRIM(IFNULL({agent_column}, '')) = ?
              ORDER BY id LIMIT 1"
        )))
        .bind(user.company_id)
        .bind(agent)
        .fetch_optional(pool)
        .await?;
        let line = rep
            .as_ref()
            .and_then(|(_, phone)| phone.as_deref())
            .map(phone10);
        (rep.map(|(id, _)| id), Some(agent.to_string()), line)
    } else {
        (
            Some(user.id),
            provider.agent_of(user),
            provider.phone_of(user),
        )
    };
    let mut agents: BTreeSet<String> = agent.into_iter().collect();
    let mut lines: BTreeSet<String> = line.into_iter().collect();
    if let Some(user_id) = user_id {
        let remembered: Vec<(String, String)> = sqlx::query_as(
            "SELECT CAST(kind AS CHAR), value FROM user_sms_identities
              WHERE company_id = ? AND user_id = ? AND (kind = 'line' OR provider = ?)",
        )
        .bind(user.company_id)
        .bind(user_id)
        .bind(provider.as_str())
        .fetch_all(pool)
        .await?;
        for (kind, value) in remembered {
            if kind == "agent" {
                agents.insert(value);
            } else {
                lines.insert(value);
            }
        }
    }
    lines.retain(|line| line.len() == 10 && !company_lines.contains(line));
    Ok(RepIds {
        user_id,
        agents: agents.into_iter().collect(),
        lines: lines.into_iter().collect(),
    })
}

/// The rows of the caller, or with `scope=all` of the filtered rep; `None`
/// when that rep has nothing of theirs on `provider`.
async fn rep_rows(
    pool: &MySqlPool,
    provider: SmsProvider,
    user: &SessionUser,
    scope: SmsScope,
    filter_agent: Option<&str>,
    company_lines: &[String],
) -> Result<Option<RepRows>, sqlx::Error> {
    if provider == SmsProvider::Telnyx {
        let (rep, lines) = telnyx_lines(pool, user, scope, filter_agent).await?;
        let ids = RepIds {
            user_id: rep,
            agents: rep.map(|id| id.to_string()).into_iter().collect(),
            lines,
        };
        return Ok(RepRows::new(ids, None));
    }
    let ids = rep_ids(pool, provider, user, scope, filter_agent, company_lines).await?;
    Ok(RepRows::new(ids, Some(company_lines.to_vec())))
}

/// Whether `user` has anything of theirs on `provider`: an agent id or
/// number they have or had (on Telnyx, a line they may call from now), or a
/// text they sent (on Telnyx, also one tagged with their user id).
pub async fn has_own(
    pool: &MySqlPool,
    provider: SmsProvider,
    user: &SessionUser,
) -> Result<bool, sqlx::Error> {
    if provider == SmsProvider::Telnyx {
        if !crate::phone::texting::inbox_lines(pool, user.company_id, user.id)
            .await?
            .is_empty()
        {
            return Ok(true);
        }
        // idx_sms_agent_threads (company_id, agent, ...).
        let tagged = exists(
            pool,
            "SELECT 1 FROM telnyx_sms s WHERE s.company_id = ? AND s.agent = ? LIMIT 1".to_string(),
            &[user.company_id.into(), user.id.to_string().into()],
        )
        .await?;
        if tagged {
            return Ok(true);
        }
    } else {
        let company_lines = company_lines(pool, provider, user.company_id).await?;
        let ids = rep_ids(pool, provider, user, SmsScope::Mine, None, &company_lines).await?;
        if !ids.agents.is_empty() || !ids.lines.is_empty() {
            return Ok(true);
        }
    }
    // idx_{provider}_sms_sender_user (sender_user_id).
    exists(
        pool,
        format!(
            "SELECT 1 FROM {} s WHERE s.sender_user_id = ? AND s.company_id = ? LIMIT 1",
            provider.sms_table()
        ),
        &[user.id.into(), user.company_id.into()],
    )
    .await
}

pub async fn company_lines(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
) -> Result<Vec<String>, sqlx::Error> {
    if provider == SmsProvider::Telnyx {
        return crate::phone::texting::company_lines(pool, company_id).await;
    }
    sqlx::query_scalar(
        "SELECT phone10 FROM sms_company_phones
          WHERE provider = ? AND company_id = ? AND agent_sender = 1 AND inbound_recipient = 1",
    )
    .bind(provider.as_str())
    .bind(company_id)
    .fetch_all(pool)
    .await
}

/// Threads carrying a row sent by the rep or tagged with an agent id of theirs.
async fn own_threads(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
    own: &RepRows,
) -> Result<Vec<String>, sqlx::Error> {
    let table = provider.sms_table();
    let mut threads = Vec::new();
    if !own.agents.is_empty() {
        // idx_sms_agent_threads (company_id, agent, phone_digits): index only.
        let mut params: Vec<Param> = vec![company_id.into()];
        params.extend(own.agents.iter().map(Param::from));
        let rows: Vec<(String,)> = super::provider::fetch_all(
            pool,
            format!(
                "SELECT DISTINCT s.phone_digits FROM {table} s
                  WHERE s.company_id = ? AND s.agent IN ({}) AND s.phone_digits IS NOT NULL",
                marks(own.agents.len())
            ),
            &params,
        )
        .await?;
        threads.extend(rows.into_iter().map(|(phone,)| phone));
    }
    if let Some(user_id) = own.sender_user_id {
        // idx_{provider}_sms_sender_user (sender_user_id).
        let rows: Vec<(String,)> = super::provider::fetch_all(
            pool,
            format!(
                "SELECT DISTINCT s.phone_digits FROM {table} s
                  WHERE s.sender_user_id = ? AND s.company_id = ? AND s.phone_digits IS NOT NULL"
            ),
            &[user_id.into(), company_id.into()],
        )
        .await?;
        threads.extend(rows.into_iter().map(|(phone,)| phone));
    }
    Ok(threads)
}

/// `(texted to one of `lines`, or from one)` over `s`, and its binds.
fn on_lines(lines: &[String]) -> (String, Vec<Param>) {
    let marks = marks(lines.len());
    let mut params: Vec<Param> = lines.iter().map(Param::from).collect();
    params.extend(lines.iter().map(Param::from));
    (
        format!(
            "((s.direction = 'inbound' AND s.recipient10 IN ({marks}))
              OR (s.direction = 'outbound' AND s.sender10 IN ({marks})))"
        ),
        params,
    )
}

async fn my_number_threads(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
    lines: &[String],
) -> Result<Vec<String>, sqlx::Error> {
    if lines.is_empty() {
        return Ok(Vec::new());
    }
    // idx_sms_inbound_recipient (company_id, direction, recipient10, ...) and
    // idx_sms_sender10 (company_id, sender10).
    let (on_lines, line_params) = on_lines(lines);
    let mut params: Vec<Param> = vec![company_id.into()];
    params.extend(line_params);
    let rows: Vec<(String,)> = super::provider::fetch_all(
        pool,
        format!(
            "SELECT DISTINCT s.phone_digits FROM {} s
              WHERE s.company_id = ? AND {on_lines}
                AND s.is_echo = 0 AND s.phone_digits IS NOT NULL",
            provider.sms_table()
        ),
        &params,
    )
    .await?;
    Ok(rows.into_iter().map(|(phone,)| phone).collect())
}

pub async fn narrowing(
    pool: &MySqlPool,
    provider: SmsProvider,
    user: &SessionUser,
    scope: SmsScope,
    filter_agent: Option<&str>,
) -> Result<Narrowing, sqlx::Error> {
    let company_lines = company_lines(pool, provider, user.company_id).await?;
    if scope == SmsScope::All && filter_agent.is_none() {
        return Ok(Narrowing {
            phones: None,
            own: None,
            company_lines,
            one_rep: false,
            line: None,
        });
    }
    let own = rep_rows(pool, provider, user, scope, filter_agent, &company_lines).await?;
    let mut phones = Vec::new();
    if let Some(own) = &own {
        phones.extend(own_threads(pool, provider, user.company_id, own).await?);
        phones.extend(my_number_threads(pool, provider, user.company_id, &own.lines).await?);
    }
    let phones: Vec<String> = phones
        .into_iter()
        .filter(|phone| !company_lines.contains(phone) && !SAMPLE_PHONES.contains(&phone.as_str()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(Narrowing {
        phones: Some(phones),
        one_rep: own.is_some(),
        own,
        company_lines,
        line: None,
    })
}

/// The rep whose Telnyx threads are visible, the caller or with
/// `scope=all` the filtered rep (`filter` = user id), and their lines
/// (last ten digits).
async fn telnyx_lines(
    pool: &MySqlPool,
    user: &SessionUser,
    scope: SmsScope,
    filter: Option<&str>,
) -> Result<(Option<i32>, Vec<String>), sqlx::Error> {
    let rep = if scope == SmsScope::All {
        filter.and_then(|id| id.parse::<i32>().ok())
    } else {
        Some(user.id)
    };
    let lines = match rep {
        Some(rep) => crate::phone::texting::inbox_lines(pool, user.company_id, rep).await?,
        None => Vec::new(),
    };
    Ok((rep, lines))
}

impl Narrowing {
    /// The predicate over `s` for the one rep's rows, and its binds.
    pub fn rep_rows(&self) -> Option<(String, Vec<Param>)> {
        self.own.as_ref().map(RepRows::visible)
    }

    /// Nothing is visible at all.
    pub fn is_empty(&self) -> bool {
        self.phones.as_ref().is_some_and(Vec::is_empty)
    }

    /// WHERE over the SMS table aliased `s` for the visible rows.
    pub fn row_filter(&self, company_id: i32) -> (String, Vec<Param>) {
        let mut parts = vec!["s.company_id = ?".to_string(), "s.is_echo = 0".to_string()];
        let mut params: Vec<Param> = vec![company_id.into()];
        if let Some(line) = &self.line {
            let (on_line, line_params) = on_lines(std::slice::from_ref(line));
            parts.push(on_line);
            params.extend(line_params);
        }
        let Some(phones) = &self.phones else {
            parts.push(format!(
                "s.phone_digits NOT IN ({})",
                marks(SAMPLE_PHONES.len())
            ));
            params.extend(SAMPLE_PHONES.iter().map(|phone| Param::from(*phone)));
            if !self.company_lines.is_empty() {
                parts.push(format!(
                    "s.phone_digits NOT IN ({})",
                    marks(self.company_lines.len())
                ));
                params.extend(self.company_lines.iter().map(Param::from));
            }
            return (parts.join(" AND "), params);
        };
        parts.push(format!("s.phone_digits IN ({})", marks(phones.len())));
        params.extend(phones.iter().map(Param::from));
        // Row by row: never another rep's texts in a thread of theirs.
        if let Some(own) = &self.own {
            let (visible, own_params) = own.visible();
            parts.push(visible);
            params.extend(own_params);
        }
        (parts.join(" AND "), params)
    }
}

/// Access to one thread, and the extra row filter inside it.
pub struct ThreadAccess {
    /// Extra predicate over `s` and its binds, e.g.
    /// `(s.agent IN (?) OR s.sender_user_id = ? OR <on a number of theirs>)`.
    pub clause: Option<(String, Vec<Param>)>,
}

impl ThreadAccess {
    /// The same access, narrowed to the texts on `line` (last ten digits).
    #[must_use]
    pub fn on_line(self, line: &str) -> Self {
        let (on_line, line_params) = on_lines(&[line.to_string()]);
        let clause = match self.clause {
            Some((clause, mut params)) => {
                params.extend(line_params);
                (format!("{clause} AND {on_line}"), params)
            }
            None => (on_line, line_params),
        };
        Self {
            clause: Some(clause),
        }
    }
}

async fn exists(pool: &MySqlPool, sql: String, params: &[Param]) -> Result<bool, sqlx::Error> {
    let rows: Vec<(i32,)> = super::provider::fetch_all(pool, sql, params).await?;
    Ok(!rows.is_empty())
}

/// Per-phone index lookups rather than the company-wide narrowing. `None`
/// when the user may not see the thread.
pub async fn thread_access(
    pool: &MySqlPool,
    provider: SmsProvider,
    user: &SessionUser,
    scope: SmsScope,
    filter_agent: Option<&str>,
    phone: &str,
) -> Result<Option<ThreadAccess>, sqlx::Error> {
    let table = provider.sms_table();
    let company: Param = user.company_id.into();
    if scope == SmsScope::All && filter_agent.is_none() {
        let found = exists(
            pool,
            format!(
                "SELECT 1 FROM {table} s WHERE s.company_id = ? AND s.phone_digits = ? LIMIT 1"
            ),
            &[company, phone.into()],
        )
        .await?;
        return Ok(found.then_some(ThreadAccess { clause: None }));
    }
    let company_lines = company_lines(pool, provider, user.company_id).await?;
    let rows = rep_rows(pool, provider, user, scope, filter_agent, &company_lines).await?;
    if let Some(rows) = rows {
        let (mine, mine_params) = rows.mine();
        let mut params = vec![company, phone.into()];
        params.extend(mine_params);
        let theirs = exists(
            pool,
            format!(
                "SELECT 1 FROM {table} s
                  WHERE s.company_id = ? AND s.phone_digits = ? AND s.is_echo = 0 AND ({})
                  LIMIT 1",
                mine.join(" OR ")
            ),
            &params,
        )
        .await?;
        if theirs {
            return Ok(Some(ThreadAccess {
                clause: Some(rows.visible()),
            }));
        }
    }
    Ok(None)
}
