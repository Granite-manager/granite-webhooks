//! Scheduling calls (event details > Scheduling call): the automatic call
//! that reads an auto-confirm text to the customer and takes a yes or a no.
//! The rules live in api-schedule (`notifications::calls`); these are
//! Telnyx's callbacks for the call our server placed.

use super::{Hook, TexmlRequest, failed, hang_up, taken, texml};
use crate::error::{ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::openapi::IntoParams;
use crate::state::AppState;
use api_schedule::routes::schedule::notifications::calls;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

/// Query of the scheduling call's callbacks.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ScheduleCallParams {
    /// The scheduling call (`schedule_calls.id`).
    pub call: i64,
}

/// Speak a scheduling call
///
/// The TeXML our scheduling call fetches when the customer picks up: a
/// `<Gather>` (a spoken word or one key) in which the voice reads the
/// scheduling text and asks for yes (1), no (2) or "repeat"; whatever comes
/// goes to `schedule-call-answer`. Hangs up when the call is over, is not
/// the company's, or an answering machine picked up (`AnsweredBy`).
#[crate::openapi::path(
    post,
    path = "/v1/webhooks/telnyx/{token}/schedule-call",
    operation_id = "speak_telnyx_schedule_call",
    tag = "telnyx-webhooks",
    params(("token" = String, Path, description = "The company's webhook token"), ScheduleCallParams),
    request_body(content = TexmlRequest, content_type = "application/x-www-form-urlencoded", description = "Telnyx's request when the customer's phone answered: `CallSid`, `AnsweredBy`"),
    responses(
        (status = 200, description = "TeXML: the message and the question, or hang up", content_type = "application/xml", body = String),
        (status = 400, description = "Not a form-encoded request, or no `call`", body = ErrorBody),
        (status = 401, description = "Telnyx's signature does not check out", body = ErrorBody),
        (status = 403, description = "The account or connection is not the company's", body = ErrorBody),
        (status = 404, description = "No company has this webhook token", body = ErrorBody),
        (status = 500, description = "Database error before the company was known", body = ErrorBody),
    )
)]
pub async fn schedule_call(
    State(state): State<AppState>,
    Path(token): Path<String>,
    MultiQuery(params): MultiQuery<ScheduleCallParams>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let hook = texml(&state, &token, &headers, &body, "schedule-call").await?;
    Ok(match ask(&state, &hook, params.call).await {
        Ok(response) => response,
        Err(err) => {
            failed("schedule-call", hook.company_id(), &err);
            hang_up()
        }
    })
}

async fn ask(state: &AppState, hook: &Hook, call_id: i64) -> ApiResult<Response> {
    hook.claim(state, "schedule-call", None).await?;
    let form = &hook.form;
    Ok(calls::ask(
        state,
        hook.company_id(),
        call_id,
        &hook.base,
        form.call_sid(),
        form.answered_by(),
    )
    .await?
    .into_response())
}

/// Take a scheduling call's answer
///
/// The `<Gather>` action of `schedule-call`: `Digits` (1 yes, 2 no, 3 repeat) or
/// `SpeechResult` (the words said). A yes turns the job Confirmed and tells
/// the Scheduling Managers; a no tells them; "repeat" reads the message
/// again; anything else (nothing too) is asked again, and after the last
/// try the customer is told a text is coming and gets the scheduling text.
#[crate::openapi::path(
    post,
    path = "/v1/webhooks/telnyx/{token}/schedule-call-answer",
    operation_id = "answer_telnyx_schedule_call",
    tag = "telnyx-webhooks",
    params(("token" = String, Path, description = "The company's webhook token"), ScheduleCallParams),
    request_body(content = TexmlRequest, content_type = "application/x-www-form-urlencoded", description = "Telnyx's `<Gather>` action request: `CallSid`, `Digits`, `SpeechResult`"),
    responses(
        (status = 200, description = "TeXML: the closing words, the question again, or hang up", content_type = "application/xml", body = String),
        (status = 400, description = "Not a form-encoded request, or no `call`", body = ErrorBody),
        (status = 401, description = "Telnyx's signature does not check out", body = ErrorBody),
        (status = 403, description = "The account or connection is not the company's", body = ErrorBody),
        (status = 404, description = "No company has this webhook token", body = ErrorBody),
        (status = 500, description = "Database error before the company was known", body = ErrorBody),
    )
)]
pub async fn schedule_call_answer(
    State(state): State<AppState>,
    Path(token): Path<String>,
    MultiQuery(params): MultiQuery<ScheduleCallParams>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let hook = texml(&state, &token, &headers, &body, "schedule-call-answer").await?;
    Ok(match answer(&state, &hook, params.call).await {
        Ok(response) => response,
        Err(err) => {
            failed("schedule-call-answer", hook.company_id(), &err);
            hang_up()
        }
    })
}

async fn answer(state: &AppState, hook: &Hook, call_id: i64) -> ApiResult<Response> {
    hook.claim(state, "schedule-call-answer", None).await?;
    let form = &hook.form;
    Ok(calls::answer(
        state,
        hook.company_id(),
        call_id,
        &hook.base,
        form.digits(),
        form.speech_result(),
    )
    .await?
    .into_response())
}

/// Hear what picked up a scheduling call
///
/// Telnyx's answering-machine detection for the call (`AnsweredBy`): for a
/// machine or a fax the call ends as unanswered, is hung up, and the
/// customer gets the scheduling text. A person changes nothing.
#[crate::openapi::path(
    post,
    path = "/v1/webhooks/telnyx/{token}/schedule-call-machine",
    operation_id = "detect_telnyx_schedule_call_machine",
    tag = "telnyx-webhooks",
    params(("token" = String, Path, description = "The company's webhook token"), ScheduleCallParams),
    request_body(content = TexmlRequest, content_type = "application/x-www-form-urlencoded", description = "Telnyx's detection result: `CallSid`, `AnsweredBy` (`human`, `machine_start`, `fax`, `unknown`)"),
    responses(
        (status = 200, description = "Taken"),
        (status = 400, description = "Not a form-encoded request, or no `call`", body = ErrorBody),
        (status = 401, description = "Telnyx's signature does not check out", body = ErrorBody),
        (status = 403, description = "The account or connection is not the company's", body = ErrorBody),
        (status = 404, description = "No company has this webhook token", body = ErrorBody),
        (status = 500, description = "Database error before the company was known", body = ErrorBody),
    )
)]
pub async fn schedule_call_machine(
    State(state): State<AppState>,
    Path(token): Path<String>,
    MultiQuery(params): MultiQuery<ScheduleCallParams>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let hook = texml(&state, &token, &headers, &body, "schedule-call-machine").await?;
    if let Err(err) = machine(&state, &hook, params.call).await {
        failed("schedule-call-machine", hook.company_id(), &err);
    }
    Ok(taken())
}

async fn machine(state: &AppState, hook: &Hook, call_id: i64) -> ApiResult<()> {
    hook.claim(state, "schedule-call-machine", None).await?;
    let form = &hook.form;
    calls::picked_up_by(
        state,
        hook.company_id(),
        call_id,
        &hook.base,
        form.call_sid(),
        form.answered_by(),
    )
    .await?;
    Ok(())
}

/// End a scheduling call
///
/// The call's status callback. An end without a yes or a no (`busy`,
/// `no-answer`, `canceled`, `failed`, or `completed` before an answer) is
/// noted on the event and the customer gets the scheduling text. Other
/// statuses change nothing.
#[crate::openapi::path(
    post,
    path = "/v1/webhooks/telnyx/{token}/schedule-call-status",
    operation_id = "end_telnyx_schedule_call",
    tag = "telnyx-webhooks",
    params(("token" = String, Path, description = "The company's webhook token"), ScheduleCallParams),
    request_body(content = TexmlRequest, content_type = "application/x-www-form-urlencoded", description = "Telnyx's status callback: `CallSid`, `CallStatus`"),
    responses(
        (status = 200, description = "Taken"),
        (status = 400, description = "Not a form-encoded request, or no `call`", body = ErrorBody),
        (status = 401, description = "Telnyx's signature does not check out", body = ErrorBody),
        (status = 403, description = "The account or connection is not the company's", body = ErrorBody),
        (status = 404, description = "No company has this webhook token", body = ErrorBody),
        (status = 500, description = "Database error before the company was known", body = ErrorBody),
    )
)]
pub async fn schedule_call_status(
    State(state): State<AppState>,
    Path(token): Path<String>,
    MultiQuery(params): MultiQuery<ScheduleCallParams>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let hook = texml(&state, &token, &headers, &body, "schedule-call-status").await?;
    if let Err(err) = end(&state, &hook, params.call).await {
        failed("schedule-call-status", hook.company_id(), &err);
    }
    Ok(taken())
}

async fn end(state: &AppState, hook: &Hook, call_id: i64) -> ApiResult<()> {
    hook.claim(state, "schedule-call-status", None).await?;
    calls::ended(state, hook.company_id(), call_id, hook.form.call_status()).await?;
    Ok(())
}
