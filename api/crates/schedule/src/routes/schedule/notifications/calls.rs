//! Event details > Scheduling call: an automatic call that reads an
//! auto-confirm text to the customer and takes their answer by fixed rules
//! (no AI): "yes" or 1 confirms the job, "no" or 2 tells the Scheduling
//! Managers, "repeat" reads the message again, and anything else is asked
//! again. It is placed once, from the Scheduling Manager's line; a call that
//! brings no yes or no (nobody picked up, voicemail, busy, a hang-up, no
//! clear answer) is followed by the same scheduling text, which then waits
//! for the customer's reply like any auto-confirm text.
//!
//! - `POST /v1/events/{id}/confirmation-call` → place the call (`place`)
//!
//! Telnyx's callbacks (`api-phone`, `telnyx_webhooks/schedule_call.rs`) run
//! it: `ask` when the customer picks up, `answer` for what they said or
//! pressed, `picked_up_by` for an answering machine, `ended` for the end.

use super::{
    AUTOMATED, Appointment, Channel, History, MAX_EDITED_TEXT, Notice, Outgoing, TemplateKind,
    appointment, bad, company_event, company_template, confirmation_failure, deliver_sent,
    failure_text, mark_called, message_user, record, render, reply_sender, sms_phone,
    start_waiting, tell_managers,
};
use crate::auth::ShopOrEmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::routes::sms::provider::SmsProvider;
use crate::state::AppState;
use crate::telnyx::{Response as Texml, Verb, VoiceCall};
use axum::Json;
use axum::extract::{Path, State};
use chrono::NaiveDateTime;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;

/// What the voice asks after the message.
pub const PROMPT: &str = "Say yes or press 1 to confirm. Say no or press 2. \
     If you'd like to repeat this information, say repeat.";
/// What it says to an answer that is no yes, no or repeat.
pub const ASK_AGAIN: &str = "Sorry, I didn't understand your answer. Please say yes or no. \
     You can also press 1 for yes, or 2 for no.";
/// What it says to a yes.
pub const YES_LINE: &str = "Thank you, your appointment is confirmed. Goodbye.";
/// What it says to a no.
pub const NO_LINE: &str = "Okay. I'll notify our scheduling manager, and they will contact you \
     to find another time. Goodbye.";
/// What it says when no try brought a yes or a no.
pub const GIVE_UP: &str =
    "Sorry, I still didn't understand. We'll send you a text message instead. Goodbye.";
/// Times the question is asked before the call gives up.
pub const MAX_TRIES: i32 = 3;
/// Times the message is read again on "repeat".
pub const MAX_REPEATS: i32 = 3;
/// How long the customer's phone rings, in seconds.
const RING_SECS: u32 = 30;
/// A call still ringing or asking after this long is taken as over.
const LIVE_MINUTES: i64 = 5;
/// The voice that speaks (Telnyx `<Say voice>`).
const VOICE: &str = "Polly.Joanna-Neural";
/// The longest spoken answer taken as a yes: longer ones say more than yes.
const MAX_YES_WORDS: usize = 6;

const YES_WORDS: [&str; 13] = [
    "yes",
    "yeah",
    "yep",
    "yup",
    "yea",
    "sure",
    "correct",
    "confirm",
    "confirmed",
    "okay",
    "ok",
    "absolutely",
    "definitely",
];
const NO_WORDS: [&str; 3] = ["no", "nope", "nah"];
const REPEAT_WORDS: [&str; 2] = ["repeat", "again"];
/// With one of these a "yes" is no plain yes ("yes, but later").
const DOUBT_WORDS: [&str; 21] = [
    "but",
    "if",
    "unless",
    "maybe",
    "not",
    "can",
    "could",
    "later",
    "earlier",
    "another",
    "different",
    "reschedule",
    "change",
    "instead",
    "what",
    "when",
    "who",
    "why",
    "how",
    "wait",
    "hold",
];

/// How the customer's answer on the call is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heard {
    Yes,
    No,
    Repeat,
    Unclear,
}

/// Reads the answer: the key pressed (1 yes, 2 no), else the words said. A
/// yes is only a short, plain yes; a no stays a no whatever comes with it;
/// both at once, or neither, is unclear.
pub fn heard(digits: Option<&str>, speech: Option<&str>) -> Heard {
    match digits.map(str::trim) {
        Some("1") => return Heard::Yes,
        Some("2") => return Heard::No,
        Some(other) if !other.is_empty() => return Heard::Unclear,
        _ => {}
    }
    // "No problem" agrees.
    let said = speech
        .unwrap_or_default()
        .to_lowercase()
        .replace("no problem", "yes");
    let words: Vec<&str> = said
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    let has = |list: &[&str]| words.iter().any(|word| list.contains(word));
    let (yes, no) = (has(&YES_WORDS), has(&NO_WORDS));
    // "Yes, repeat" asks for the message again: it confirms nothing.
    if has(&REPEAT_WORDS) {
        Heard::Repeat
    } else if yes && !no && !has(&DOUBT_WORDS) && words.len() <= MAX_YES_WORDS {
        Heard::Yes
    } else if no && !yes {
        Heard::No
    } else {
        Heard::Unclear
    }
}

/// The scheduling text as the voice reads it: without the sentences that ask
/// for a reply by text ("Please reply YES to confirm..."), which `PROMPT`
/// replaces.
pub fn spoken(text: &str) -> String {
    let mut sentences: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        current.push(c);
        let ends =
            matches!(c, '.' | '!' | '?') && chars.peek().is_none_or(|next| next.is_whitespace());
        if ends || c == '\n' {
            sentences.push(std::mem::take(&mut current));
        }
    }
    sentences.push(current);
    sentences
        .iter()
        .filter(|sentence| {
            !sentence
                .to_lowercase()
                .split(|c: char| !c.is_alphanumeric())
                .any(|word| word == "reply")
        })
        .flat_map(|sentence| sentence.split_whitespace())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether Telnyx says an answering machine (or a fax) picked up.
fn machine(answered_by: Option<&str>) -> bool {
    answered_by.is_some_and(|by| {
        let by = by.trim().to_ascii_lowercase();
        by.starts_with("machine") || by == "fax"
    })
}

/// How the company places a Scheduling call.
pub struct Caller {
    /// The TeXML app our server calls through.
    app_id: String,
    /// The Scheduling Manager's line, E.164.
    from: String,
    /// The root of the company's Telnyx callbacks.
    base: String,
}

/// How `company_id` places a Scheduling call: calling is on, and the texts
/// that wait for an answer go out from a Granite Manager line (the
/// Scheduling Manager's), which the call comes from too. `None` otherwise.
pub async fn caller(state: &AppState, company_id: i32) -> Result<Option<Caller>, sqlx::Error> {
    let pool = &state.pool;
    let Some(config) = crate::phone::company::load(pool, company_id).await? else {
        return Ok(None);
    };
    if !config.can_place_calls() {
        return Ok(None);
    }
    let Some(app_id) = config.texml_app_id.clone() else {
        return Ok(None);
    };
    let Ok((SmsProvider::Telnyx, _, seat)) = reply_sender(pool, company_id).await? else {
        return Ok(None);
    };
    Ok(seat.line.map(|line| Caller {
        app_id,
        from: line.e164,
        base: config.webhook_base(&state.phone),
    }))
}

/// Body of `POST /v1/events/{id}/confirmation-call`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CallInput {
    /// An auto-confirm text of the company.
    pub template_id: i32,
    /// The text as edited before calling. Left out, the template goes as it
    /// is.
    #[serde(default)]
    pub text: Option<String>,
}

/// `POST /v1/events/{id}/confirmation-call`.
#[derive(Debug, Serialize, ToSchema)]
pub struct CallPlaced {
    /// The 10 digits being called.
    pub phone: String,
}

/// Call the customer to confirm
///
/// Places one automatic call to the event's customer from the Scheduling
/// Manager's line. A voice reads the auto-confirm text (without its "reply
/// YES" sentence) and asks for a yes or a no: "yes" or 1 turns the job
/// Confirmed and tells the Scheduling Managers, "no" or 2 tells them the
/// customer said no, "repeat" reads it again, anything else is asked again
/// (3 tries). The job turns Awaiting confirmation. When the call brings no
/// yes or no, the same text is sent to the customer as a scheduling text.
#[crate::openapi::path(
    post,
    path = "/v1/events/{id}/confirmation-call",
    operation_id = "call_event_confirmation",
    tag = "schedule",
    params(("id" = i32, Path, description = "Event id")),
    request_body = CallInput,
    responses(
        (status = 200, description = "The call is ringing", body = CallPlaced),
        (status = 400, description = "Unscheduled, no customer or phone, not an auto-confirm text, calls are not set up, or a call is already going", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, admin or shop manager or worker", body = ErrorBody),
        (status = 404, description = "No such event in the caller's company", body = ErrorBody),
        (status = 502, description = "The call could not be placed", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn place(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<CallInput>,
) -> ApiResult<Json<CallPlaced>> {
    let pool = &state.pool;
    let company_id = user.company_id;
    let stored = company_event(pool, company_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if stored.unscheduled {
        return Err(bad(
            "Put the event on the calendar before calling the customer",
        ));
    }
    let template = company_template(pool, company_id, input.template_id)
        .await?
        .filter(|template| {
            Channel::parse(&template.channel) == Channel::Sms
                && TemplateKind::parse(&template.kind) == TemplateKind::Question
        })
        .ok_or_else(|| bad("Pick an auto-confirm text"))?;
    let event = appointment(pool, id).await?;
    let Some(customer_id) = event.customer_id else {
        return Err(bad("The event has no customer"));
    };
    let edited = match input.text.as_deref().map(str::trim) {
        Some("") => return Err(bad("Write the text first")),
        Some(text) if text.chars().count() > MAX_EDITED_TEXT => {
            return Err(bad("The text is too long"));
        }
        other => other,
    };
    let Some(caller) = caller(&state, company_id).await? else {
        return Err(bad(
            "Calls need a Granite Manager phone line for the Scheduling Manager. Ask your admin to set one up.",
        ));
    };
    let Some(phone) = sms_phone(pool, company_id, &event, customer_id).await? else {
        return Err(bad("The customer has no phone number"));
    };
    let message = Outgoing {
        company_id,
        channel: Channel::Sms,
        subject: None,
        body: edited.unwrap_or(&template.body),
        idempotency_key: None,
        by_hand: true,
        awaits_reply: true,
    };
    let user_id = message_user(pool, company_id, &event, customer_id).await?;
    let body = match render(pool, &message, &event, customer_id, user_id).await? {
        Ok((_, body)) => body,
        Err(code) => return Err(confirmation_failure(code)),
    };
    let says = spoken(&body);
    if says.is_empty() {
        return Err(bad("Write what the call says first"));
    }
    let live: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM schedule_calls
          WHERE event_id = ? AND status IN ('calling', 'asking')
            AND placed_at > UTC_TIMESTAMP() - INTERVAL ? MINUTE",
    )
    .bind(id)
    .bind(LIVE_MINUTES)
    .fetch_one(pool)
    .await?;
    if live > 0 {
        return Err(bad("A call to this customer is already going"));
    }
    let call_id = sqlx::query(
        "INSERT INTO schedule_calls
           (company_id, event_id, template_id, start_date, phone, body, placed_by, placed_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, UTC_TIMESTAMP())",
    )
    .bind(company_id)
    .bind(id)
    .bind(template.id)
    .bind(event.start_date)
    .bind(&phone)
    .bind(&body)
    .bind(user.id)
    .execute(pool)
    .await?
    .last_insert_id();
    let base = &caller.base;
    let placed = state
        .telnyx
        .texml_voice_call(
            &caller.app_id,
            &VoiceCall {
                to: &format!("+1{phone}"),
                from: &caller.from,
                url: &format!("{base}/schedule-call?call={call_id}"),
                status_callback: &format!("{base}/schedule-call-status?call={call_id}"),
                amd_callback: &format!("{base}/schedule-call-machine?call={call_id}"),
                ring_secs: RING_SECS,
            },
        )
        .await;
    let sid = match placed {
        Ok(sid) => sid,
        Err(err) => {
            tracing::warn!(error = %err, event_id = id, "scheduling call not placed");
            sqlx::query("DELETE FROM schedule_calls WHERE id = ?")
                .bind(call_id)
                .execute(pool)
                .await?;
            return Err(ApiError::Upstream(
                "The call could not be placed. Try again.".into(),
            ));
        }
    };
    if let Some(sid) = sid {
        sqlx::query("UPDATE schedule_calls SET call_sid = ? WHERE id = ? AND call_sid IS NULL")
            .bind(sid)
            .bind(call_id)
            .execute(pool)
            .await?;
    }
    record(
        pool,
        &History::new(id, company_id, "updated", user.name.as_deref())
            .message(&format!("Confirmation call placed: {}", template.name))
            .body(&says),
    )
    .await;
    mark_called(pool, company_id, id, user.name.as_deref(), true, "called").await?;
    Ok(Json(CallPlaced { phone }))
}

/// A call placed.
#[derive(sqlx::FromRow)]
struct Call {
    id: i32,
    company_id: i32,
    event_id: i32,
    template_id: Option<i32>,
    start_date: NaiveDateTime,
    body: String,
    placed_by: Option<i32>,
    call_sid: Option<String>,
    tries: i32,
    repeats: i32,
    status: String,
}

async fn load(pool: &MySqlPool, company_id: i32, id: i64) -> Result<Option<Call>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, company_id, event_id, template_id, start_date, body, placed_by, call_sid,
                tries, repeats, CAST(status AS CHAR) AS status
           FROM schedule_calls WHERE id = ? AND company_id = ?",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(pool)
    .await
}

fn hang_up() -> Texml {
    Texml::new().hangup()
}

fn say(text: &str) -> Verb {
    Verb::new("Say").attr("voice", VOICE).text(text)
}

/// `text`, then listen for a word or a key; whatever comes (nothing too)
/// goes to `schedule-call-answer`.
fn question(base: &str, call_id: i32, text: &str) -> Texml {
    let action = format!("{base}/schedule-call-answer?call={call_id}");
    let gather = Verb::new("Gather")
        .attr("input", "dtmf speech")
        .attr("numDigits", "1")
        .attr("timeout", "6")
        .attr("speechTimeout", "2")
        .attr("language", "en-US")
        .attr("hints", "yes,no,repeat")
        .attr("action", action.clone())
        .child(say(text));
    Texml::new().verb(gather).redirect(&action)
}

/// The message and the question, as the voice reads them.
fn message(call: &Call) -> String {
    format!("{} {PROMPT}", spoken(&call.body))
}

/// The customer's phone picked up (the call's `Url`): the voice reads the
/// message and asks. A machine hears nothing: the call ends as unanswered.
pub async fn ask(
    state: &AppState,
    company_id: i32,
    call_id: i64,
    base: &str,
    sid: Option<&str>,
    answered_by: Option<&str>,
) -> Result<Texml, sqlx::Error> {
    let pool = &state.pool;
    let Some(call) = load(pool, company_id, call_id).await? else {
        return Ok(hang_up());
    };
    if machine(answered_by) {
        no_answer(state, &call, "unanswered", "voicemail picked up").await?;
        return Ok(hang_up());
    }
    let started = sqlx::query(
        "UPDATE schedule_calls SET status = 'asking', tries = 1,
                call_sid = COALESCE(call_sid, ?)
          WHERE id = ? AND status = 'calling'",
    )
    .bind(sid)
    .bind(call.id)
    .execute(pool)
    .await?
    .rows_affected()
        == 1;
    // A repeat of this request while the question is open asks it again; a
    // call that is over hangs up.
    if !started && call.status != "asking" {
        return Ok(hang_up());
    }
    Ok(question(base, call.id, &message(&call)))
}

/// What the customer said or pressed (the `<Gather>` action): a yes
/// confirms the job, a no tells the Scheduling Managers, "repeat" reads the
/// message again, anything else is asked again until the tries run out.
pub async fn answer(
    state: &AppState,
    company_id: i32,
    call_id: i64,
    base: &str,
    digits: Option<&str>,
    speech: Option<&str>,
) -> Result<Texml, sqlx::Error> {
    let pool = &state.pool;
    let Some(call) = load(pool, company_id, call_id).await? else {
        return Ok(hang_up());
    };
    if call.status != "asking" {
        return Ok(hang_up());
    }
    let said: String = digits
        .or(speech)
        .unwrap_or_default()
        .trim()
        .chars()
        .take(255)
        .collect();
    match heard(digits, speech) {
        Heard::Yes => {
            if !close(pool, &call, "yes", Some(&said)).await? {
                return Ok(hang_up());
            }
            confirm(state, &call).await?;
            Ok(Texml::new().verb(say(YES_LINE)).hangup())
        }
        Heard::No => {
            if !close(pool, &call, "no", Some(&said)).await? {
                return Ok(hang_up());
            }
            record(
                pool,
                &History::new(call.event_id, call.company_id, "updated", Some(AUTOMATED))
                    .message("The customer said no on the confirmation call"),
            )
            .await;
            tell_managers(
                state,
                &notice(state, &call).await?,
                "the customer said NO on the confirmation call",
            )
            .await?;
            Ok(Texml::new().verb(say(NO_LINE)).hangup())
        }
        Heard::Repeat if call.repeats < MAX_REPEATS => {
            sqlx::query("UPDATE schedule_calls SET repeats = repeats + 1, heard = ? WHERE id = ?")
                .bind(&said)
                .bind(call.id)
                .execute(pool)
                .await?;
            Ok(question(base, call.id, &message(&call)))
        }
        Heard::Repeat | Heard::Unclear if call.tries < MAX_TRIES => {
            sqlx::query("UPDATE schedule_calls SET tries = tries + 1, heard = ? WHERE id = ?")
                .bind(&said)
                .bind(call.id)
                .execute(pool)
                .await?;
            Ok(question(base, call.id, ASK_AGAIN))
        }
        Heard::Repeat | Heard::Unclear => {
            no_answer(state, &call, "unclear", "no clear answer").await?;
            Ok(Texml::new().verb(say(GIVE_UP)).hangup())
        }
    }
}

/// Telnyx says what picked up. An answering machine: the call ends as
/// unanswered (the text goes out) and is hung up. True when it was one.
pub async fn picked_up_by(
    state: &AppState,
    company_id: i32,
    call_id: i64,
    base: &str,
    sid: Option<&str>,
    answered_by: Option<&str>,
) -> Result<bool, sqlx::Error> {
    if !machine(answered_by) {
        return Ok(false);
    }
    let Some(call) = load(&state.pool, company_id, call_id).await? else {
        return Ok(false);
    };
    no_answer(state, &call, "unanswered", "voicemail picked up").await?;
    // The call is over for us: its next instructions hang it up.
    if let Some(sid) = call.call_sid.as_deref().or(sid) {
        let url = format!("{base}/schedule-call?call={}", call.id);
        if let Err(err) = state.telnyx.texml_redirect(sid, &url).await {
            tracing::warn!(error = %err, call_id = call.id, "scheduling call not hung up");
        }
    }
    Ok(true)
}

/// The call ended (its final `CallStatus`): without a yes or a no it is
/// unanswered, and the text goes out.
pub async fn ended(
    state: &AppState,
    company_id: i32,
    call_id: i64,
    call_status: Option<&str>,
) -> Result<(), sqlx::Error> {
    let Some(call) = load(&state.pool, company_id, call_id).await? else {
        return Ok(());
    };
    let why = match call_status.map(str::trim) {
        Some("busy") => "the line was busy",
        Some("failed") => "the call did not go through",
        Some("completed") if call.status == "asking" => "the customer hung up before answering",
        Some("no-answer" | "canceled" | "completed") => "nobody picked up",
        _ => return Ok(()),
    };
    no_answer(state, &call, "unanswered", why).await
}

/// Ends the call as `status` when it is still open; true when this did it.
async fn close(
    pool: &MySqlPool,
    call: &Call,
    status: &str,
    heard: Option<&str>,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE schedule_calls
            SET status = ?, heard = COALESCE(?, heard), ended_at = UTC_TIMESTAMP()
          WHERE id = ? AND status IN ('calling', 'asking')",
    )
    .bind(status)
    .bind(heard)
    .bind(call.id)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

/// The notice about `call`'s job to the Scheduling Managers.
async fn notice(state: &AppState, call: &Call) -> Result<Notice, sqlx::Error> {
    let event = appointment(&state.pool, call.event_id).await?;
    Ok(Notice {
        company_id: call.company_id,
        event_id: call.event_id,
        customer_id: event.customer_id,
        sender: call.placed_by,
        actor: "Call",
    })
}

/// The customer said yes: the job turns Confirmed (unless it already is, or
/// it moved since the call was placed) and the Scheduling Managers hear
/// about it.
async fn confirm(state: &AppState, call: &Call) -> Result<(), sqlx::Error> {
    let pool = &state.pool;
    let changed = sqlx::query(
        "UPDATE events SET status = 'confirmed'
          WHERE id = ? AND start_date = ? AND deleted_date IS NULL
            AND COALESCE(status, '') <> 'confirmed'",
    )
    .bind(call.event_id)
    .bind(call.start_date)
    .execute(pool)
    .await?
    .rows_affected()
        == 1;
    let line = if changed {
        "Status changed to Confirmed: the customer said yes on the call"
    } else {
        "The customer said yes on the confirmation call"
    };
    record(
        pool,
        &History::new(call.event_id, call.company_id, "updated", Some(AUTOMATED)).message(line),
    )
    .await;
    if changed {
        tell_managers(
            state,
            &notice(state, call).await?,
            "is Confirmed: the customer said yes on the call",
        )
        .await?;
    }
    Ok(())
}

/// A line of the job's history, written by the call.
const fn note<'a>(call: &Call, line: &'a str) -> History<'a> {
    History::new(call.event_id, call.company_id, "updated", Some(AUTOMATED)).message(line)
}

/// The call brought no yes or no (`why`): once, it is noted on the event
/// and the same text goes to the customer as a scheduling text, which then
/// waits for their reply (AI reads it, follow-ups go out). Nothing is texted
/// for a job that was deleted, moved or Confirmed meanwhile.
async fn no_answer(
    state: &AppState,
    call: &Call,
    status: &str,
    why: &str,
) -> Result<(), sqlx::Error> {
    let pool = &state.pool;
    if !close(pool, call, status, None).await? {
        return Ok(());
    }
    record(pool, &note(call, &format!("Confirmation call: {why}"))).await;
    let current: Option<(bool, NaiveDateTime, String)> = sqlx::query_as(
        "SELECT deleted_date IS NOT NULL, start_date, COALESCE(status, '') FROM events WHERE id = ?",
    )
    .bind(call.event_id)
    .fetch_optional(pool)
    .await?;
    let waits = current.is_some_and(|(deleted, start, status)| {
        !deleted && start == call.start_date && status != "confirmed"
    });
    if !waits {
        return Ok(());
    }
    let event: Appointment = appointment(pool, call.event_id).await?;
    let Some(customer_id) = event.customer_id else {
        return Ok(());
    };
    let message = Outgoing {
        company_id: call.company_id,
        channel: Channel::Sms,
        subject: None,
        body: &call.body,
        idempotency_key: Some(format!("schedule-call-{}", call.id)),
        by_hand: true,
        awaits_reply: true,
    };
    let outcome = match deliver_sent(state, &message, &event, customer_id).await {
        Ok(outcome) => outcome,
        Err(err) => {
            tracing::error!(error = %err, call_id = call.id, "text after a scheduling call failed");
            Err("send_error")
        }
    };
    let sent = match outcome {
        Ok(sent) => sent,
        Err(code) => {
            let line = format!(
                "Scheduling text not sent after the call: {}",
                failure_text(code).unwrap_or("the text failed to send")
            );
            record(pool, &note(call, &line)).await;
            return Ok(());
        }
    };
    record(
        pool,
        &note(call, "Scheduling text sent: the call brought no answer").body(&sent.content),
    )
    .await;
    let template = match call.template_id {
        Some(id) => company_template(pool, call.company_id, id).await?,
        None => None,
    };
    if let (Some(template), Some(text)) = (template, sent.text) {
        start_waiting(
            pool,
            call.company_id,
            &event,
            &template,
            &text,
            call.placed_by,
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_plain_words_are_read() {
        assert_eq!(heard(Some("1"), None), Heard::Yes);
        assert_eq!(heard(Some("2"), None), Heard::No);
        assert_eq!(heard(Some("7"), Some("yes")), Heard::Unclear);
        for yes in [
            "Yes.",
            "yeah",
            "Yes, that works",
            "ok",
            "Sure!",
            "No problem",
        ] {
            assert_eq!(heard(None, Some(yes)), Heard::Yes, "{yes}");
        }
        for no in [
            "No.",
            "nope",
            "No, can we do another day?",
            "no I can't make it",
        ] {
            assert_eq!(heard(None, Some(no)), Heard::No, "{no}");
        }
        for repeat in ["Repeat", "repeat please", "say that again", "Yes, repeat"] {
            assert_eq!(heard(None, Some(repeat)), Heard::Repeat, "{repeat}");
        }
    }

    #[test]
    fn anything_else_is_unclear() {
        for unclear in [
            "",
            "maybe",
            "who is this",
            "yes but can you come later",
            "yes if it is after two",
            "yes no",
            "ues",
            "yes I think so let me check with my husband first okay",
            "hello",
        ] {
            assert_eq!(heard(None, Some(unclear)), Heard::Unclear, "{unclear}");
        }
        assert_eq!(heard(None, None), Heard::Unclear);
        assert_eq!(heard(Some(" "), None), Heard::Unclear);
    }

    #[test]
    fn the_voice_skips_the_reply_by_text_sentence() {
        assert_eq!(
            spoken(
                "Hi Travis, this is Granite Depot. Can we come Thursday, November 19? \
                 The window is 11:30 AM \u{2013} 1:00 PM. Please reply YES to confirm, or NO and we'll find another time."
            ),
            "Hi Travis, this is Granite Depot. Can we come Thursday, November 19? \
             The window is 11:30 AM \u{2013} 1:00 PM."
        );
        assert_eq!(spoken("Reply YES or NO."), "");
        assert_eq!(spoken("See you at 9.\nThanks"), "See you at 9. Thanks");
    }

    #[test]
    fn machines_are_told_from_people() {
        assert!(machine(Some("machine_start")));
        assert!(machine(Some("machine_end_beep")));
        assert!(machine(Some("fax")));
        assert!(!machine(Some("human")));
        assert!(!machine(Some("unknown")));
        assert!(!machine(None));
    }
}
