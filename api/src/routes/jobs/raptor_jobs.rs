//! `POST /v1/jobs/raptor-jobs`: each new project of a company with a Raptor
//! login becomes a Raptor job (LPI Cloud, templator.com), filled the way the
//! office does by hand. See `crate::raptor`.

use crate::auth::job::JobCaller;
use crate::error::{ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::Serialize;
use std::time::Duration;

/// Stop starting projects after this long; one takes a few seconds.
const TIME_BUDGET: Duration = Duration::from_mins(2);

/// What a run did.
#[derive(Debug, Default, Serialize, ToSchema)]
pub struct RaptorJobsRun {
    /// Projects whose Raptor jobs were made.
    pub done: u32,
    /// Projects that failed this time (tried again later, three times in all).
    pub failed: u32,
}

/// Make the Raptor jobs of new projects
///
/// For each company with a Raptor login, the projects created since the login
/// was saved that have no Raptor job yet: their jobs are added to Raptor with
/// the job info, material, sink cutouts and the "Notes" note.
#[crate::openapi::path(
    post,
    path = "/v1/jobs/raptor-jobs",
    operation_id = "run_raptor_jobs",
    tag = "jobs",
    responses(
        (status = 200, description = "How many projects were sent", body = RaptorJobsRun),
        (status = 401, description = "Missing or wrong job key", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("job_key" = []))
)]
pub async fn run(State(state): State<AppState>, _job: JobCaller) -> ApiResult<Json<RaptorJobsRun>> {
    let config = crate::raptor::sync::Config::from_env();
    let summary = crate::raptor::sync::run(&state.pool, TIME_BUDGET, &config).await?;
    Ok(Json(RaptorJobsRun {
        done: summary.done,
        failed: summary.failed,
    }))
}
