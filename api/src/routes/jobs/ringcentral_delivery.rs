//! `POST /v1/jobs/ringcentral-delivery`: marks the texts `RingCentral` did
//! not deliver (blocked as spam, refused by the carrier) as failed, for texts
//! sent before the webhook marked them as they were rejected.
//!
//! Run by hand, one rep's extension and one page of 100 texts at a time
//! (a Lambda lives 30 s); call again with `next_page` until it is null.

use crate::auth::job::JobCaller;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use common::telephony::ringcentral::{mark_undelivered, sent_texts_page};
use serde::{Deserialize, Serialize};

/// Which texts to check.
#[derive(Debug, Deserialize, ToSchema)]
pub struct RingcentralDeliveryInput {
    pub company_id: i32,
    /// The rep's `RingCentral` extension id (`ringcentral_sms.agent`).
    pub extension: String,
    /// Texts sent from then on (ISO 8601, e.g. `2026-06-01T00:00:00Z`).
    pub date_from: String,
    /// And before then, when given.
    pub date_to: Option<String>,
    /// Page of 100 texts, from 1.
    pub page: Option<u32>,
}

/// What a run did.
#[derive(Debug, Default, Serialize, ToSchema)]
pub struct RingcentralDeliveryRun {
    /// Texts `RingCentral` reported on this page.
    pub checked: usize,
    /// Of them, the CRM texts now marked failed.
    pub marked: usize,
    /// The page to ask for next, or null when this was the last.
    pub next_page: Option<u32>,
}

/// Mark texts RingCentral did not deliver
///
/// Reads one page of the texts a rep sent through `RingCentral` and marks
/// the CRM's copy of each one `RingCentral` did not deliver as failed, with
/// the reason (marked as spam, number flagged as spam, or undelivered).
#[crate::openapi::path(
    post,
    path = "/v1/jobs/ringcentral-delivery",
    operation_id = "run_ringcentral_delivery",
    tag = "jobs",
    request_body = RingcentralDeliveryInput,
    responses(
        (status = 200, description = "What was done", body = RingcentralDeliveryRun),
        (status = 401, description = "Missing or wrong job key", body = ErrorBody),
        (status = 502, description = "RingCentral refused the request", body = ErrorBody),
    ),
    security(("job_key" = []))
)]
pub async fn run(
    State(state): State<AppState>,
    _job: JobCaller,
    Json(input): Json<RingcentralDeliveryInput>,
) -> ApiResult<Json<RingcentralDeliveryRun>> {
    let page = input.page.unwrap_or(1).max(1);
    let (records, more) = sent_texts_page(
        &state.pool,
        &state.telephony_http,
        input.company_id,
        &input.extension,
        &input.date_from,
        input.date_to.as_deref(),
        page,
    )
    .await
    .map_err(|error| {
        ApiError::Upstream(format!(
            "RingCentral refused the request ({}): {}",
            error.status,
            error.message.unwrap_or_default()
        ))
    })?;
    let mut marked = 0;
    for record in &records {
        if mark_undelivered(&state.pool, input.company_id, &input.extension, record).await? {
            marked += 1;
        }
    }
    Ok(Json(RingcentralDeliveryRun {
        checked: records.len(),
        marked,
        next_page: more.then_some(page + 1),
    }))
}
