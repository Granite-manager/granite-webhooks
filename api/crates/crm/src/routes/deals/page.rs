//! Everything the deal page shows on open, in one request.
//!
//! The page used to read six routes at once (`/v1/deals/{id}`,
//! `/stage-history`, `/activities`, `/notes`, `/emails`,
//! `/document-events`); each request costs a Lambda and a database
//! connection. Those routes stay: the phone app uses them, and the page
//! refreshes single parts through them after a change.

use super::documents::{self, DocumentEvent};
use super::queries;
use super::schemas::{Activity, DealDetail, Note, StageVisit};
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::routes::emails::deal_history::{self, DealEmails};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use lambda_http::tracing;
use serde::Serialize;

/// The deal page's data.
#[derive(Debug, Serialize, ToSchema)]
pub struct DealPage {
    /// The deal, as `GET /v1/deals/{id}` returns it.
    pub deal: DealDetail,
    /// Stage visits, oldest first, as `GET /v1/deals/{id}/stage-history`.
    pub stage_history: Vec<StageVisit>,
    /// Live activities, newest first, as `GET /v1/deals/{id}/activities`.
    pub activities: Vec<Activity>,
    /// Live notes with comments, newest first, as `GET /v1/deals/{id}/notes`.
    pub notes: Vec<Note>,
    /// The email history, as `GET /v1/deals/{id}/emails`. Null when it could
    /// not be read; the rest of the page is still returned.
    pub emails: Option<DealEmails>,
    /// Quote and contract events, unsorted, as
    /// `GET /v1/deals/{id}/document-events`. Null when they could not be read.
    pub document_events: Option<Vec<DocumentEvent>>,
}

/// Get everything the deal page shows
///
/// One read instead of six: the deal, its stage history, activities, notes,
/// email history and quote/contract events, each shaped like its own route.
/// `emails` and `document_events` are null when they fail to load, so the
/// page still opens. To refresh one part after a change, call its own route
/// (`/v1/deals/{id}/notes`, `/activities`, ...).
#[crate::openapi::path(
    get,
    path = "/v1/deals/{id}/page",
    operation_id = "get_deal_page",
    tag = "deals",
    params(("id" = i64, Path, description = "Deal id")),
    responses(
        (status = 200, description = "The deal page's data", body = DealPage),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such live deal in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<DealPage>> {
    let pool = &state.pool;
    let deal = queries::find_deal(pool, user.company_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let stage_history = queries::stage_history(pool, id).await?;
    let activities = queries::list_activities(pool, user.company_id, id).await?;
    let notes = queries::list_notes(pool, user.company_id, id).await?;

    let emails = deal_history::deal_emails(
        State(state.clone()),
        EmployeeUser(user.clone()),
        Path(id),
    )
    .await
    .map(|Json(emails)| emails)
    .inspect_err(|error| tracing::error!(?error, deal_id = id, "deal page emails failed"))
    .ok();
    let document_events = documents::list(State(state), EmployeeUser(user), Path(id))
        .await
        .map(|Json(events)| events)
        .inspect_err(|error| {
            tracing::error!(?error, deal_id = id, "deal page document events failed");
        })
        .ok();

    Ok(Json(DealPage {
        deal,
        stage_history,
        activities,
        notes,
        emails,
        document_events,
    }))
}
