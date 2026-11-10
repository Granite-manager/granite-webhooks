//! The wording of a company's own invoices, learned by the AI.
//!
//! A draft made from a quote lists every item of the quote. An office then
//! tidies it the same way every time: lines its invoices never show are
//! deleted, others are reworded to the names it uses in `QuickBooks`.
//! [`super::rules`] learns such a change once it was made to the very same
//! line three times. The AI sees the pattern sooner and wider: it reads the
//! company's newest `QuickBooks` invoices next to what became of past draft
//! lines and proposes rules ("leave out lines with fabrication", "finished
//! edge is called edge"), in the rule language of [`super::instructions`].
//!
//! The AI only proposes. Each rule is then checked here, without AI, against
//! the same invoices and outcomes ([`Evidence::check`]): a line the office
//! keeps is never left out, a wording must be the company's own. Only rules
//! that pass are kept, and only `hide` and `rename`: like every learned
//! rule they never change an amount or leave out a priced line, the editor
//! offers left-out lines back, and an admin's `keep` instruction overrules
//! them. No AI runs when an invoice is drafted.
//!
//! It is learned again at most once a day, in the background, when someone
//! opens Create invoice (`style_due` of the draft), or when an admin asks.

use super::instructions::{self, Action, InstructionRule, phrase_regex};
use super::refuse_installer;
use super::rules::{self, SampleLine, Wording};
use crate::ai::{Caller, Chat, MINI_FALLBACK_MODEL, MINI_MODEL};
use crate::auth::{AdminUser, EmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use chrono::{DateTime, SecondsFormat, Utc};
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::time::Duration;

/// Most rules kept.
const MAX_RULES: usize = 20;
/// Fewest invoices a wording is learned from.
const MIN_INVOICES: usize = 5;
/// Fewest invoices that say a kind of line is never shown: with fewer, a
/// line may be missing only because no recent job had one.
const NEVER_SHOWN_FROM: usize = 30;
/// Characters of invoice lines shown to the AI.
const SAMPLE_BUDGET: usize = 24_000;
/// Past draft lines read, and kinds of line shown to the AI.
const PAST_LINES: u32 = 2000;
const CORRECTIONS: usize = 60;
/// Lines of the open draft shown to the AI.
const DRAFT_LINES: usize = 60;
/// The newest outcomes of a kind of line that say what the office does now.
const TALLIED: usize = 20;
/// Hours before it is learned again.
const FRESH_HOURS: i64 = 20;
/// The API answers within 30 seconds.
const AI_TIMEOUT: Duration = Duration::from_secs(25);

const SYSTEM_PROMPT: &str = r#"You study how one countertop company writes its QuickBooks invoices, so that the invoices its CRM drafts come out the way its own office writes them.

Background: the CRM drafts an invoice from a quote. It writes one priced line per part of the job and then every item of the quote as a text line under it. The office then tidies the draft by hand: it deletes kinds of lines its invoices never show, and rewords lines to the names it uses in QuickBooks. Your task is to find the tidying this office does every time, so the CRM can do it for them. A wrong rule costs them a correction on every invoice from then on, so write a rule only when the data clearly shows it. A few sure rules are worth more than many guesses, and no rules at all is a fine answer.

You get one JSON object:
- "invoices": recent invoices exactly as the office wrote them in QuickBooks, newest first. Each is a list of lines with "text", and with "product" and "priced": true when the line charges an amount. This is the style to match.
- "corrections": what became of lines the CRM drafted earlier. "drafted" is one example of that kind of line, "kept" and "removed" count how often the office kept or deleted such a line, "reworded_to" lists what they changed it to. This is the strongest evidence. It is empty for a company that has not used the CRM for invoices yet; then compare "draft_lines" with "invoices".
- "draft_lines": lines as the CRM drafts them now.
Quantities, square feet, lengths and prices differ on every job; they are examples, never part of a pattern.

Write rules with these two actions only:
- {"type": "hide", "line": "<phrase>"}: text lines containing the phrase are left out of every draft. Use it for a kind of line the office deletes (removed much more often than kept), or that the drafts always have while none of the office's own invoices shows anything like it.
- {"type": "rename", "line": "<phrase>", "to": "<their wording>"}: the phrase is replaced inside the lines that contain it, and the rest of the line (quantity, color, model) stays. Use it when the office consistently uses another name for the same thing.

How phrases work: a phrase matches at the start of a word, in any case, so "cabinet" also matches "Cabinets". Pick the shortest phrase that names that kind of line and no other kind: "fabrication" or "finished edge", never "73.3 sq ft Fabrication". Take the phrase from "drafted" or "draft_lines", because that is the text it must match.
A rule can hold on some quotes only: "when_has" lists phrases the draft must contain and "when_lacks" phrases it must not contain (the fabrication line is deleted only on quotes with cabinets: "when_has": ["cabinet"]). Leave both empty for a rule that always holds.

Your rules are checked against the same data after you answer, and a rule that fails is dropped:
- Never hide a kind of line the corrections show the office keeps, or one that appears in the office's own invoices.
- "to" must be wording found in "invoices" or in "reworded_to".
- Never touch amounts, quantities or prices, and never hide a priced line.
- No rules about products, classes, addresses, dates or emails: those are learned elsewhere.

Return only JSON in this shape:
{"rules": [{"when_has": [], "when_lacks": [], "action": {"type": "hide", "line": "fabrication"}, "evidence": "<one short sentence: what in the data shows it>"}], "notes": []}
"notes" holds at most 3 short sentences for the company's admin about a pattern you saw that these two actions cannot express. If nothing is clearly supported, return {"rules": [], "notes": []}."#;

/// `GET`/`POST /v1/qbo/invoice-style`.
#[derive(Debug, Default, Clone, Serialize, Deserialize, ToSchema)]
pub struct InvoiceStyle {
    /// The rules every new invoice draft follows, each checked against the
    /// company's invoices: `hide` and `rename` only.
    #[serde(default)]
    pub rules: Vec<InstructionRule>,
    /// What the AI noticed and could not make a rule of.
    #[serde(default)]
    pub notes: Vec<String>,
    /// When it was last learned (UTC); null before the first time.
    #[serde(default)]
    pub learned_at: Option<String>,
    /// How many of the company's `QuickBooks` invoices it was learned from.
    #[serde(default)]
    pub invoices_read: usize,
}

/// Body of `POST /v1/qbo/invoice-style`.
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct LearnInput {
    /// Learn now even when it was learned today (admins only; ignored for
    /// others).
    #[serde(default)]
    pub force: bool,
    /// The lines of the draft that is open, as the quote makes them: the
    /// kinds of line the rules are about.
    #[serde(default)]
    pub lines: Vec<String>,
}

/// What is kept in `qbo_invoice_style.rules_json`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Stored {
    #[serde(default)]
    rules: Vec<InstructionRule>,
    #[serde(default)]
    notes: Vec<String>,
    #[serde(default)]
    invoices_read: usize,
    /// The rules the check dropped, with why (for support; not shown).
    #[serde(default)]
    dropped: Vec<String>,
}

/// The learned rules, and whether it is time to learn again.
#[derive(Debug, Default)]
pub(super) struct Learned {
    pub(super) rules: Vec<InstructionRule>,
    pub(super) stale: bool,
}

async fn stored(
    pool: &MySqlPool,
    company_id: i32,
) -> sqlx::Result<Option<(Stored, DateTime<Utc>)>> {
    let row: Option<(String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT CAST(rules_json AS CHAR), learned_at FROM qbo_invoice_style WHERE company_id = ?",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(json, at)| (serde_json::from_str(&json).unwrap_or_default(), at)))
}

fn fresh(learned_at: DateTime<Utc>) -> bool {
    Utc::now() - learned_at < chrono::Duration::hours(FRESH_HOURS)
}

/// The company's learned rules; none before the first time, or when they
/// cannot be read (a draft is then made without them).
pub(super) async fn load(pool: &MySqlPool, company_id: i32) -> Option<Learned> {
    match stored(pool, company_id).await {
        Ok(found) => found.map(|(stored, at)| Learned {
            rules: stored.rules,
            stale: !fresh(at),
        }),
        Err(err) => {
            tracing::warn!(error = %err, company_id, "invoice style not read");
            None
        }
    }
}

/// What is kept, as the API says it.
async fn read(pool: &MySqlPool, company_id: i32) -> sqlx::Result<InvoiceStyle> {
    Ok(stored(pool, company_id)
        .await?
        .map(|(stored, at)| style(stored, at))
        .unwrap_or_default())
}

fn style(stored: Stored, at: DateTime<Utc>) -> InvoiceStyle {
    InvoiceStyle {
        rules: stored.rules,
        notes: stored.notes,
        learned_at: Some(at.to_rfc3339_opts(SecondsFormat::Secs, true)),
        invoices_read: stored.invoices_read,
    }
}

/// What became of one past draft line (newest first when listed).
#[derive(Debug, Clone, sqlx::FromRow)]
pub(super) struct Past {
    /// The invoice it was drafted for.
    pub(super) qbo_invoice_id: Option<String>,
    /// The line as drafted.
    pub(super) draft_text: String,
    /// `kept`, `changed` or `removed`.
    pub(super) outcome: String,
    /// What it was changed to, its numbers as `{0}`, `{1}`...
    pub(super) saved_value: Option<String>,
}

impl Past {
    /// What a changed line reads now.
    fn reworded(&self) -> Option<String> {
        let to = self
            .saved_value
            .as_deref()
            .filter(|_| self.outcome == "changed")?;
        Some(rules::fill(to, &self.draft_text).unwrap_or_else(|| to.to_string()))
    }
}

async fn past_lines(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Vec<Past>> {
    sqlx::query_as(
        "SELECT qbo_invoice_id, draft_text, outcome, saved_value FROM qbo_invoice_draft_outcomes
          WHERE company_id = ? AND field = 'line' AND draft_text IS NOT NULL
          ORDER BY id DESC LIMIT ?",
    )
    .bind(company_id)
    .bind(PAST_LINES)
    .fetch_all(pool)
    .await
}

/// What became of one kind of drafted line, for the AI.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
struct Correction {
    drafted: String,
    kept: usize,
    removed: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    reworded_to: Vec<String>,
}

/// The past lines by kind (the same line but for its numbers), the kinds
/// seen most recently first.
fn corrections(past: &[Past]) -> Vec<Correction> {
    let mut kinds: Vec<(String, Correction)> = Vec::new();
    for line in past {
        let kind = rules::shape(&line.draft_text);
        let at = kinds
            .iter()
            .position(|(known, _)| *known == kind)
            .unwrap_or_else(|| {
                kinds.push((
                    kind,
                    Correction {
                        drafted: line.draft_text.clone(),
                        ..Correction::default()
                    },
                ));
                kinds.len() - 1
            });
        let correction = &mut kinds[at].1;
        match line.outcome.as_str() {
            "kept" => correction.kept += 1,
            "removed" => correction.removed += 1,
            _ => {
                if let Some(to) = line.reworded()
                    && correction.reworded_to.len() < 3
                    && !correction.reworded_to.contains(&to)
                {
                    correction.reworded_to.push(to);
                }
            }
        }
    }
    kinds
        .into_iter()
        .map(|(_, correction)| correction)
        .take(CORRECTIONS)
        .collect()
}

/// The words (letters only, three or more) a phrase must find in an invoice.
fn tokens(phrase: &str) -> Vec<String> {
    rules::words(std::iter::once(phrase))
}

/// What the AI's rules are checked against.
pub(super) struct Evidence<'a> {
    /// The words of each of the company's scanned invoices.
    pub(super) invoice_words: &'a [Vec<String>],
    /// What became of past draft lines, newest first.
    pub(super) past: &'a [Past],
}

/// How often the office kept, removed and reworded such lines lately.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Tally {
    kept: usize,
    removed: usize,
    reworded: usize,
}

impl Tally {
    const fn all(self) -> usize {
        self.kept + self.removed + self.reworded
    }

    /// The office mostly leaves such lines as drafted.
    const fn keeps(self) -> bool {
        self.kept >= 3 && self.kept * 2 >= self.all()
    }
}

impl Evidence<'_> {
    /// The company's invoices that have every word of the phrase (at the
    /// start of a word, as a phrase matches).
    fn invoices_with(&self, phrase: &str) -> usize {
        let tokens = tokens(phrase);
        if tokens.is_empty() {
            return 0;
        }
        self.invoice_words
            .iter()
            .filter(|words| {
                tokens
                    .iter()
                    .all(|token| words.iter().any(|word| word.starts_with(token.as_str())))
            })
            .count()
    }

    /// More than a few of the company's own invoices say it.
    fn in_their_invoices(&self, phrase: &str) -> bool {
        let with = self.invoices_with(phrase);
        with >= 2 && with * 20 > self.invoice_words.len()
    }

    /// What the office did lately to the draft lines with the phrase, on
    /// the quotes the rule holds on.
    fn tally(&self, rule: &InstructionRule, phrase: &str) -> Tally {
        let mut tally = Tally::default();
        let Some(re) = phrase_regex(phrase) else {
            return tally;
        };
        // Each invoice's draft lines: the quote the rule must hold on.
        let mut quotes: HashMap<&str, Vec<String>> = HashMap::new();
        for line in self.past {
            if let Some(invoice) = line.qbo_invoice_id.as_deref() {
                quotes
                    .entry(invoice)
                    .or_default()
                    .push(line.draft_text.clone());
            }
        }
        let conditional = !rule.when_has.is_empty() || !rule.when_lacks.is_empty();
        for line in self
            .past
            .iter()
            .filter(|line| re.is_match(&line.draft_text))
            .filter(|line| {
                !conditional
                    || line
                        .qbo_invoice_id
                        .as_deref()
                        .and_then(|invoice| quotes.get(invoice))
                        .is_some_and(|texts| {
                            !instructions::holding(std::slice::from_ref(rule), texts).is_empty()
                        })
            })
            .take(TALLIED)
        {
            match line.outcome.as_str() {
                "kept" => tally.kept += 1,
                "removed" => tally.removed += 1,
                _ => tally.reworded += 1,
            }
        }
        tally
    }

    /// Whether the data bears the rule out; else why not.
    pub(super) fn check(&self, rule: &InstructionRule) -> Result<(), String> {
        let scanned = self.invoice_words.len();
        match &rule.action {
            Action::Hide { line } => {
                if tokens(line).is_empty() {
                    return Err("the phrase names no word".into());
                }
                let tally = self.tally(rule, line);
                if tally.keeps() {
                    return Err(format!(
                        "the office kept such lines {} of {} times",
                        tally.kept,
                        tally.all()
                    ));
                }
                let removes = tally.removed >= 3 && tally.removed >= tally.kept * 3;
                if !removes && self.in_their_invoices(line) {
                    return Err("the company's own invoices have such lines".into());
                }
                let never_shown = scanned >= NEVER_SHOWN_FROM && self.invoices_with(line) == 0;
                if tally.removed >= 2 || never_shown {
                    Ok(())
                } else {
                    Err("too little shows the office leaves such lines out".into())
                }
            }
            Action::Rename { line, to } => {
                if tokens(line).is_empty() || tokens(to).is_empty() {
                    return Err("the phrase or the new wording names no word".into());
                }
                if line.trim().eq_ignore_ascii_case(to.trim()) {
                    return Err("nothing changes".into());
                }
                let tally = self.tally(rule, line);
                let wanted = to.trim().to_lowercase();
                let reworded_so = phrase_regex(line).map_or(0, |re| {
                    self.past
                        .iter()
                        .filter(|past| re.is_match(&past.draft_text))
                        .filter_map(Past::reworded)
                        .filter(|now| now.to_lowercase().contains(&wanted))
                        .count()
                });
                if reworded_so < 2 {
                    if tally.keeps() {
                        return Err("the office keeps this wording".into());
                    }
                    if self.in_their_invoices(line) {
                        return Err("the company's own invoices use this wording".into());
                    }
                    if self.invoices_with(to) < 2 {
                        return Err("the new wording is not in the company's invoices".into());
                    }
                }
                Ok(())
            }
            _ => Err("only leaving out and rewording lines is learned".into()),
        }
    }
}

/// The rules of the AI's reply that the evidence bears out, and why the
/// others were dropped.
pub(super) fn checked(reply: &Value, evidence: &Evidence) -> (Vec<InstructionRule>, Vec<String>) {
    let (proposed, mut dropped) = instructions::compile(reply, &[], &[]);
    let mut rules: Vec<InstructionRule> = Vec::new();
    for rule in proposed {
        if rules.contains(&rule) {
            continue;
        }
        match evidence.check(&rule) {
            Ok(()) if rules.len() < MAX_RULES => rules.push(rule),
            Ok(()) => dropped.push(format!("{}: over {MAX_RULES} rules", json!(rule.action))),
            Err(why) => dropped.push(format!("{}: {why}", json!(rule.action))),
        }
    }
    (rules, dropped)
}

fn notes(reply: &Value) -> Vec<String> {
    reply["notes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|note| note.as_str().map(str::trim))
        .filter(|note| !note.is_empty())
        .map(|note| note.chars().take(300).collect())
        .take(3)
        .collect()
}

/// The newest sampled invoices that fit the budget.
fn within_budget(samples: &[Vec<SampleLine>]) -> Vec<&Vec<SampleLine>> {
    let mut left = SAMPLE_BUDGET;
    samples
        .iter()
        .take_while(|lines| {
            let size: usize = lines.iter().map(|line| line.text.len() + 24).sum();
            let fits = size <= left;
            left = left.saturating_sub(size);
            fits
        })
        .collect()
}

/// The open draft's lines, then the kinds of line drafted before.
fn draft_lines(open: &[String], corrections: &[Correction]) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for text in open
        .iter()
        .map(String::as_str)
        .chain(corrections.iter().map(|c| c.drafted.as_str()))
    {
        let text: String = text.trim().chars().take(200).collect();
        if !text.is_empty() && !lines.contains(&text) {
            lines.push(text);
        }
        if lines.len() >= DRAFT_LINES {
            break;
        }
    }
    lines
}

async fn keep(pool: &MySqlPool, company_id: i32, stored: &Stored) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO qbo_invoice_style (company_id, rules_json) VALUES (?, ?)
         ON DUPLICATE KEY UPDATE rules_json = VALUES(rules_json), learned_at = CURRENT_TIMESTAMP",
    )
    .bind(company_id)
    .bind(json!(stored).to_string())
    .execute(pool)
    .await
    .map(|_| ())
}

/// Reads the company's invoices, asks the AI for rules, checks and keeps them.
async fn learn(
    state: &AppState,
    company_id: i32,
    user_id: i32,
    open: &[String],
) -> ApiResult<InvoiceStyle> {
    let wording: Wording = rules::wording(state, company_id)
        .await?
        .ok_or_else(|| ApiError::Conflict("QuickBooks is not connected".into()))?;
    let invoices_read = wording.invoice_words.len().max(wording.samples.len());
    if wording.samples.len() < MIN_INVOICES {
        // Nothing to learn from yet: said once a day, not asked each time.
        let stored = Stored {
            invoices_read,
            notes: vec![format!(
                "QuickBooks has {} invoices with lines so far; at least {MIN_INVOICES} are needed to learn how you write them.",
                wording.samples.len()
            )],
            ..Stored::default()
        };
        keep(&state.pool, company_id, &stored).await?;
        return Ok(read(&state.pool, company_id).await?);
    }
    let past = past_lines(&state.pool, company_id).await?;
    let corrections = corrections(&past);
    let prompt = json!({
        "invoices": within_budget(&wording.samples),
        "corrections": corrections,
        "draft_lines": draft_lines(open, &corrections),
    })
    .to_string();
    let chat = |model| {
        let mut chat = Chat::new(model)
            .message("system", SYSTEM_PROMPT)
            .message("user", &prompt);
        chat.json = true;
        chat.reasoning_effort = Some("low");
        chat.timeout = AI_TIMEOUT;
        chat
    };
    let caller = Caller {
        feature: "qbo_invoice_style",
        user_id: Some(user_id),
        company_id: Some(company_id),
    };
    let reply = match state.ai.chat(&state.pool, caller, &chat(MINI_MODEL)).await {
        Err(err) if err.is_model_permission() => {
            state
                .ai
                .chat(&state.pool, caller, &chat(MINI_FALLBACK_MODEL))
                .await
        }
        reply => reply,
    }
    .map_err(|err| {
        ApiError::Upstream(format!(
            "The AI could not read the invoices just now ({err}). What was learned before stays; try again."
        ))
    })?;
    let reply: Value = serde_json::from_str(reply.trim()).map_err(|_| {
        ApiError::Upstream(
            "The AI answered with something other than rules. What was learned before stays; try again."
                .into(),
        )
    })?;
    let evidence = Evidence {
        invoice_words: &wording.invoice_words,
        past: &past,
    };
    let (rules, dropped) = checked(&reply, &evidence);
    if !dropped.is_empty() {
        tracing::info!(company_id, dropped = ?dropped, "invoice style rules dropped by the check");
    }
    let stored = Stored {
        rules,
        notes: notes(&reply),
        invoices_read,
        dropped,
    };
    keep(&state.pool, company_id, &stored).await?;
    Ok(read(&state.pool, company_id).await?)
}

/// Read what the AI learned from the company's invoices
///
/// The rules every new invoice draft follows because the company's own
/// `QuickBooks` invoices are written that way (lines left out, lines
/// reworded), when they were learned and from how many invoices. Empty
/// before the first time. The invoice instructions win over these.
#[crate::openapi::path(
    get,
    path = "/v1/qbo/invoice-style",
    operation_id = "get_qbo_invoice_style",
    tag = "quickbooks",
    responses(
        (status = 200, description = "What was learned", body = InvoiceStyle),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_style(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<InvoiceStyle>> {
    Ok(Json(read(&state.pool, user.company_id).await?))
}

/// Learn how the company writes its invoices
///
/// The AI reads the company's newest `QuickBooks` invoices and what became
/// of earlier invoice drafts (also what the office changed in `QuickBooks`
/// afterwards), and proposes which lines to leave out and how to reword
/// others. Each rule is checked against those invoices before it is kept,
/// and new drafts then follow the kept ones. Learned at most once in 20
/// hours: sooner, what was learned is returned as it is, unless an admin
/// sends `force`. Meant to be called in the background when Create invoice
/// opens (`style_due` of `POST /v1/quotes/{id}/invoice-draft`); it can take
/// up to 25 seconds.
#[crate::openapi::path(
    post,
    path = "/v1/qbo/invoice-style",
    operation_id = "learn_qbo_invoice_style",
    tag = "quickbooks",
    request_body = LearnInput,
    responses(
        (status = 200, description = "What was learned", body = InvoiceStyle),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin, or is an installer", body = ErrorBody),
        (status = 409, description = "QuickBooks is not connected", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "The AI failed; what was learned before stays", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn learn_style(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<LearnInput>,
) -> ApiResult<Json<InvoiceStyle>> {
    refuse_installer(&state, &user).await?;
    let company_id = user.company_id;
    let force = input.force && (user.is_admin || user.is_superuser);
    if !force
        && let Some((stored, at)) = stored(&state.pool, company_id).await?
        && fresh(at)
    {
        return Ok(Json(style(stored, at)));
    }
    learn(&state, company_id, user.id, &input.lines)
        .await
        .map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn past(invoice: &str, text: &str, outcome: &str, value: Option<&str>) -> Past {
        Past {
            qbo_invoice_id: Some(invoice.into()),
            draft_text: text.into(),
            outcome: outcome.into(),
            saved_value: value.map(Into::into),
        }
    }

    /// Three countertop invoices and one with cabinets, newest first. The
    /// office deletes Fabrication and Laser Template, rewords the edge, keeps
    /// the sink, and keeps Installation except with cabinets.
    fn history() -> Vec<Past> {
        let mut lines = Vec::new();
        for (invoice, sqft) in [("14", "59.2"), ("13", "40"), ("12", "73.3")] {
            lines.extend([
                past(
                    invoice,
                    &format!("{sqft} sq ft Fabrication"),
                    "removed",
                    None,
                ),
                past(
                    invoice,
                    "33.5 lin ft Finished Edge - Eased",
                    "changed",
                    Some("Eased Edge {0} lf"),
                ),
                past(invoice, "Laser Template", "removed", None),
                past(invoice, "1 Sink - 60/40", "kept", None),
                past(invoice, &format!("{sqft} sq ft Installation"), "kept", None),
            ]);
        }
        for invoice in ["11", "10"] {
            lines.extend([
                past(invoice, "12 Shaker Cabinets - White", "kept", None),
                past(invoice, "30 sq ft Installation", "removed", None),
            ]);
        }
        lines
    }

    /// The company's own invoices: edges, sinks and sealer; never
    /// fabrication or a template.
    fn invoices() -> Vec<Vec<String>> {
        (0..30)
            .map(|n| {
                let mut lines = vec!["Kitchen based @ 59.2 sqft", "Eased Edge 33.5 lf"];
                lines.push(if n % 2 == 0 {
                    "Sink 60/40"
                } else {
                    "Free sealer"
                });
                rules::words(lines.into_iter())
            })
            .collect()
    }

    fn rule(has: &[&str], action: Action) -> InstructionRule {
        InstructionRule {
            when_has: has.iter().map(|p| (*p).to_string()).collect(),
            when_lacks: Vec::new(),
            action,
        }
    }

    fn hide(line: &str) -> Action {
        Action::Hide { line: line.into() }
    }

    fn rename(line: &str, to: &str) -> Action {
        Action::Rename {
            line: line.into(),
            to: to.into(),
        }
    }

    #[test]
    fn a_rule_is_kept_only_when_the_invoices_bear_it_out() {
        let (past, invoice_words) = (history(), invoices());
        let evidence = Evidence {
            invoice_words: &invoice_words,
            past: &past,
        };
        let check = |has: &[&str], action| evidence.check(&rule(has, action));
        // Deleted every time, and in none of their invoices.
        assert_eq!(check(&[], hide("fabrication")), Ok(()));
        assert_eq!(check(&[], hide("laser template")), Ok(()));
        // The office keeps sinks, and its own invoices have them.
        assert!(check(&[], hide("sink")).unwrap_err().contains("kept"));
        // Kept on countertop quotes, deleted with cabinets: only the
        // conditional rule holds.
        assert!(check(&[], hide("installation")).is_err());
        assert_eq!(check(&["cabinet"], hide("installation")), Ok(()));
        // A phrase without a word names no kind of line.
        assert!(check(&[], hide("60/40")).is_err());
        // Their wording: reworded so three times.
        assert_eq!(
            check(&[], rename("finished edge - eased", "Eased Edge")),
            Ok(())
        );
        // A wording of the AI's own, or one that changes nothing.
        assert!(
            check(&[], rename("laser template", "Digital measure"))
                .unwrap_err()
                .contains("not in the company's invoices")
        );
        assert!(check(&[], rename("sink", "Sink")).is_err());
        assert!(check(&[], rename("sink", "Basin")).is_err());
        // Anything but hiding and rewording is not learned.
        assert!(
            check(
                &[],
                Action::AddLine {
                    text: "Thank you".into()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn a_company_new_to_the_crm_learns_from_its_invoices_alone() {
        let invoice_words = invoices();
        let evidence = Evidence {
            invoice_words: &invoice_words,
            past: &[],
        };
        // None of 30 invoices mentions fabrication; half mention a sink.
        assert_eq!(evidence.check(&rule(&[], hide("fabrication"))), Ok(()));
        assert!(evidence.check(&rule(&[], hide("sink"))).is_err());
        // "Edge" is how they say it; "finished edge" they never write.
        assert_eq!(
            evidence.check(&rule(&[], rename("finished edge", "Edge"))),
            Ok(())
        );
        // Too few invoices to say what they never show: maybe no recent job
        // had one.
        let few = Evidence {
            invoice_words: &invoice_words[..12],
            past: &[],
        };
        assert!(few.check(&rule(&[], hide("fabrication"))).is_err());
    }

    #[test]
    fn the_ai_reply_is_checked_rule_by_rule() {
        let (past, invoice_words) = (history(), invoices());
        let evidence = Evidence {
            invoice_words: &invoice_words,
            past: &past,
        };
        let reply = json!({
            "rules": [
                { "when_has": [], "when_lacks": [], "action": { "type": "hide", "line": "Fabrication" },
                  "evidence": "removed 3 of 3 times" },
                { "action": { "type": "hide", "line": "fabrication" } },
                { "action": { "type": "hide", "line": "sink" } },
                { "action": { "type": "rename", "line": "finished edge - eased", "to": "Eased Edge" } },
                { "action": { "type": "cc", "email": "office@example.com" } },
                { "action": { "type": "product", "product": "Sales" } },
            ],
            "notes": ["Invoices end with a thank-you line.", " ", 7],
        });
        let (rules, dropped) = checked(&reply, &evidence);
        assert_eq!(
            rules
                .iter()
                .map(|rule| rule.action.clone())
                .collect::<Vec<_>>(),
            [
                hide("fabrication"),
                rename("finished edge - eased", "Eased Edge")
            ]
        );
        assert_eq!(dropped.len(), 3);
        assert_eq!(notes(&reply), ["Invoices end with a thank-you line."]);
    }

    #[test]
    fn the_ai_sees_each_kind_of_line_once_with_what_became_of_it() {
        let found = corrections(&history());
        assert_eq!(found.len(), 6);
        assert_eq!(
            found[0],
            Correction {
                drafted: "59.2 sq ft Fabrication".into(),
                kept: 0,
                removed: 3,
                reworded_to: Vec::new(),
            }
        );
        assert_eq!(found[1].reworded_to, ["Eased Edge 33.5 lf"]);
        let installation = found
            .iter()
            .find(|c| c.drafted.ends_with("Installation"))
            .unwrap();
        assert_eq!((installation.kept, installation.removed), (3, 2));
        // The open draft's lines come first, each once.
        let open = ["1 Sink - 60/40".to_string(), " New line ".to_string()];
        let lines = draft_lines(&open, &found);
        assert_eq!(
            &lines[..3],
            ["1 Sink - 60/40", "New line", "59.2 sq ft Fabrication"]
        );
        assert_eq!(lines.len(), 7);
    }
}
