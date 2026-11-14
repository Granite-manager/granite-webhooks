//! `GET /v1/quotes/square-feet` → many quotes' countertop sq ft in one call.
//!
//! The schedule's month view writes each day's sq ft of Mark as Sold jobs in
//! its corner. It used to open every sold quote whole (`GET /v1/quotes/{id}`,
//! a dozen queries each) all at once, and on Lambda each of those calls took
//! a database connection of its own: a month of jobs ran the database out of
//! connections ("database error"). This answers all of them with one query.

use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::openapi::{IntoParams, ToSchema};
use crate::quote_pricing::doc::QuoteDoc;
use crate::quote_pricing::price_list;
use crate::quote_pricing::takeoff::{area_takeoff, round_takeoff};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{MySql, QueryBuilder};
use std::collections::{BTreeMap, BTreeSet};

/// Most quotes one call takes.
pub const MAX_QUOTES: usize = 200;

/// Query of `GET /v1/quotes/square-feet`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SquareFeetQuery {
    /// Quote ids, repeated (`?ids=12&ids=40`); 1 to 200 of them. Repeats are
    /// counted once.
    #[serde(default)]
    pub ids: Vec<i32>,
}

/// `GET /v1/quotes/square-feet`.
#[derive(Debug, Serialize, ToSchema)]
pub struct QuotesSquareFeet {
    /// Countertop sq ft by quote id: every area of the quote's drawing, each
    /// rounded as its price list rounds material (to 0.1 sq ft, or up to whole
    /// feet), times the area's quantity. 0 for a quote with no drawing. Quotes
    /// that are deleted or not the caller's company's are left out.
    pub square_feet: BTreeMap<i32, f64>,
}

#[derive(sqlx::FromRow)]
struct QuoteSquareFeetRow {
    id: i32,
    drawing: Option<String>,
    snapshot: Option<String>,
    revision_info: Option<String>,
}

/// The price list a quote is shown with (`pricingForQuote`): the snapshot it
/// was made with, else its price list revision. Only its settings matter here.
fn quote_price_list(row: &QuoteSquareFeetRow) -> Option<Value> {
    let parsed = |text: Option<&str>| {
        text.and_then(|t| serde_json::from_str::<Value>(t).ok())
            .filter(|v| !v.is_null())
            .and_then(|v| price_list::parse(&v))
    };
    parsed(row.snapshot.as_deref()).or_else(|| parsed(row.revision_info.as_deref()))
}

/// The quote's countertop sq ft, as the CRM's `areaSquareFeet` sums them.
#[allow(clippy::cast_precision_loss)] // an area's quantity is a small count
fn quote_square_feet(row: &QuoteSquareFeetRow) -> f64 {
    let Some(doc) = row
        .drawing
        .as_deref()
        .and_then(|t| serde_json::from_str::<Value>(t).ok())
        .and_then(|v| QuoteDoc::parse(&v))
    else {
        return 0.0;
    };
    let sq_ft_up = quote_price_list(row)
        .is_some_and(|pl| pl.get("materialRounding").and_then(Value::as_str) == Some("up"));
    doc.areas.iter().fold(0.0, |sum, area| {
        let rounded = round_takeoff(&area_takeoff(&doc, &area.id), sq_ft_up);
        // `area.quantity || 1`
        let quantity = if area.quantity == 0 { 1 } else { area.quantity };
        (quantity as f64).mul_add(rounded.counter_sq_ft, sum)
    })
}

/// Get the countertop sq ft of many quotes
///
/// Each quote's countertop square feet, summed over the areas of its drawing
/// exactly as the quote's Project panel shows them, for many quotes in one
/// call: the schedule adds up the sold quotes of a day's template and
/// installation jobs. Pass the ids as repeated `ids` (1 to 200). Quotes that
/// are deleted or belong to another company are left out of the answer, never
/// an error. For a whole quote (drawing, prices, revisions) call
/// `GET /v1/quotes/{id}` instead.
#[crate::openapi::path(
    get,
    path = "/v1/quotes/square-feet",
    operation_id = "list_quotes_square_feet",
    tag = "quotes",
    params(SquareFeetQuery),
    responses(
        (status = 200, description = "Sq ft by quote id", body = QuotesSquareFeet),
        (status = 400, description = "No ids, more than 200, or an id that is not a number", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_quotes_square_feet(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<SquareFeetQuery>,
) -> ApiResult<Json<QuotesSquareFeet>> {
    let ids: BTreeSet<i32> = query.ids.into_iter().collect();
    if ids.is_empty() {
        return Err(ApiError::BadRequest("Pass at least one quote id as `ids`.".into()));
    }
    if ids.len() > MAX_QUOTES {
        return Err(ApiError::BadRequest(format!(
            "At most {MAX_QUOTES} quotes at a time."
        )));
    }
    let mut sql: QueryBuilder<MySql> = QueryBuilder::new(
        "SELECT q.id, CAST(q.draft_drawing_json AS CHAR) AS drawing,
                CAST(q.price_snapshot_json AS CHAR) AS snapshot,
                CASE WHEN pl.id IS NOT NULL THEN CAST(r.info_json AS CHAR) END AS revision_info
           FROM quotes q
           LEFT JOIN price_list_revisions r ON r.id = q.price_list_revision_id
           LEFT JOIN price_lists pl ON pl.id = r.price_list_id AND pl.company_id = q.company_id
          WHERE q.company_id = ",
    );
    sql.push_bind(user.company_id);
    sql.push(" AND q.deleted_at IS NULL AND q.id IN (");
    let mut list = sql.separated(", ");
    for id in &ids {
        list.push_bind(*id);
    }
    sql.push(") ORDER BY q.id");
    let rows: Vec<QuoteSquareFeetRow> = sql.build_query_as().fetch_all(&state.pool).await?;
    Ok(Json(QuotesSquareFeet {
        square_feet: rows
            .iter()
            .map(|row| (row.id, quote_square_feet(row)))
            .collect(),
    }))
}
