//! `POST /v1/jobs/company-taxes`: each company's sales tax looked up again
//! from its address once a month (and soon after a lookup that got no
//! answer), so a new state, county or city rate reaches it with nobody
//! typing it. One company per run (see `company_tax::refresh_due`).

use crate::auth::job::JobCaller;
use crate::company_tax;
use crate::error::{ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::Serialize;

/// What a run did.
#[derive(Debug, Default, Serialize, ToSchema)]
pub struct CompanyTaxesRun {
    /// Companies looked up (0 or 1).
    pub processed: usize,
    /// Companies whose rate changed.
    pub changed: usize,
}

/// Refresh company sales taxes
///
/// Looks up the sales tax of the company checked longest ago (never, or
/// over 30 days ago) from its address and saves it.
#[crate::openapi::path(
    post,
    path = "/v1/jobs/company-taxes",
    operation_id = "run_company_taxes",
    tag = "jobs",
    responses(
        (status = 200, description = "How many companies were looked up", body = CompanyTaxesRun),
        (status = 401, description = "Missing or wrong job key", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("job_key" = []))
)]
pub async fn run(
    State(state): State<AppState>,
    _job: JobCaller,
) -> ApiResult<Json<CompanyTaxesRun>> {
    let run = company_tax::refresh_due(&state).await?;
    Ok(Json(CompanyTaxesRun {
        processed: run.checked,
        changed: run.changed,
    }))
}
