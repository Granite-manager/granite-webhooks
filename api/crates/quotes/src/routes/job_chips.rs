//! What a schedule job card shows of its project, for many jobs at once.
//!
//! - `GET /v1/schedule/job-chips?ids=` → each job's stones and sq ft
//!
//! The "My schedule" cards used to work these out one card at a time: the
//! event's sales (`GET /v1/events/{id}/sale`), each sale (`GET /v1/sales/{id}`
//! and `GET /v1/transactions/{id}`), the customer's quotes (`GET /v1/quotes`)
//! and each sold quote whole (`GET /v1/quotes/{id}`), about five requests per
//! card. This answers the same in one. Those routes stay.

use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::openapi::{IntoParams, ToSchema};
use crate::openapi::{OpenApiRouter, routes};
use crate::routes::events::project_notes::{find_event_sales, street_key};
use crate::routes::quotes::quotes_square_feet;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use sqlx::{MySql, MySqlPool, QueryBuilder};
use std::collections::{HashMap, HashSet};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(job_chips))
}

/// Most events one request takes.
const MAX_EVENTS: usize = 100;

/// Quote statuses that mean sold (`mark_quote_sold`).
const SOLD: [&str; 2] = ["accepted", "converted"];

/// Query of `GET /v1/schedule/job-chips`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct JobChipsQuery {
    /// Event ids (`ids=1&ids=2`), at most 100.
    #[serde(default)]
    pub ids: Vec<i32>,
}

/// One job's stones and sq ft.
#[derive(Debug, Serialize, ToSchema, PartialEq)]
pub struct JobChips {
    /// The event.
    pub event_id: i32,
    /// Stone names of the project's sales (newest sale first, each sale's
    /// slabs in the order they were added), each once. Empty when the event
    /// has no sale, or one of its sales can't be read.
    pub stones: Vec<String>,
    /// The project's countertop sq ft: its sold quotes' (as
    /// `GET /v1/quotes/square-feet` counts them), else its sales' rooms'.
    /// 0 when unknown. Not rounded.
    pub square_feet: f64,
}

/// List schedule jobs' stones and sq ft
///
/// For each of these events of the company, what its card on "My schedule"
/// shows: the stones of its project's sales and its sq ft. The project is the
/// event's sales as `GET /v1/events/{id}/sale` finds them. The sq ft is that
/// of the customer's sold quotes at the event's address (a customer whose
/// sold quotes are all one job lends them when there is no street to
/// compare), else the rooms of the project's sales. Events of other
/// companies, deleted ones and unknown ids are left out. Open one job's
/// whole project with `GET /v1/events/{id}/sale` and `GET /v1/sales/{id}`.
#[crate::openapi::path(
    get,
    path = "/v1/schedule/job-chips",
    operation_id = "list_schedule_job_chips",
    tag = "schedule",
    params(JobChipsQuery),
    responses(
        (status = 200, description = "Each found event's stones and sq ft, in id order", body = [JobChips]),
        (status = 400, description = "More than 100 events asked for", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn job_chips(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<JobChipsQuery>,
) -> ApiResult<Json<Vec<JobChips>>> {
    let mut ids = query.ids;
    ids.sort_unstable();
    ids.dedup();
    if ids.len() > MAX_EVENTS {
        return Err(ApiError::BadRequest(format!(
            "at most {MAX_EVENTS} events at once"
        )));
    }
    Ok(Json(load(&state.pool, user.company_id, &ids).await?))
}

#[derive(sqlx::FromRow)]
struct EventRow {
    id: i32,
    sale_id: Option<i32>,
    customer_id: Option<i64>,
    location: Option<String>,
}

#[derive(sqlx::FromRow)]
struct SlabRow {
    sale_id: i32,
    stone_name: Option<String>,
    parent_id: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct RoomSlabRow {
    sale_id: i32,
    room_id: Option<String>,
    square_feet: Option<String>,
}

#[derive(sqlx::FromRow)]
struct QuoteRow {
    id: i32,
    customer_id: i64,
    status: Option<String>,
    address: Option<String>,
}

/// One sold-quote candidate, for [`drawing_quotes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoldCandidate<'a> {
    pub id: i32,
    pub status: &'a str,
    pub address: Option<&'a str>,
}

fn key_of(address: Option<&str>) -> Option<String> {
    street_key(address.unwrap_or(""))
}

/// The event's sold quotes (the website's `drawingQuotes`).
///
/// Those at the event's address. Without an address to compare, a customer
/// whose sold quotes are all one job shows them. Never an unsold quote, and
/// never a builder's other jobs.
pub fn drawing_quotes(quotes: &[SoldCandidate<'_>], location: Option<&str>) -> Vec<i32> {
    let sold: Vec<&SoldCandidate<'_>> = quotes
        .iter()
        .filter(|quote| SOLD.contains(&quote.status))
        .collect();
    let key = key_of(location);
    if let Some(key) = &key {
        let here: Vec<i32> = sold
            .iter()
            .filter(|quote| key_of(quote.address).as_ref() == Some(key))
            .map(|quote| quote.id)
            .collect();
        if !here.is_empty() {
            return here;
        }
    }
    let keys: HashSet<Option<String>> = sold.iter().map(|quote| key_of(quote.address)).collect();
    if keys.len() != 1 {
        return Vec::new();
    }
    let only = keys.into_iter().next().flatten();
    let all = || sold.iter().map(|quote| quote.id).collect();
    match only {
        None if sold.len() == 1 => all(),
        None => Vec::new(),
        Some(_) if key.is_some() => Vec::new(),
        Some(_) => all(),
    }
}

/// The website's `Number(text) || 0`.
fn js_number(text: Option<&str>) -> f64 {
    let text = text.unwrap_or("").trim();
    if text.is_empty() {
        return 0.0;
    }
    text.parse::<f64>().ok().filter(|v| v.is_finite()).unwrap_or(0.0)
}

fn in_list(sql: &str, column: &str, ids: &[i32]) -> QueryBuilder<MySql> {
    let mut query = QueryBuilder::<MySql>::new(sql);
    crate::sql::push_in(&mut query, column, ids);
    query
}

#[allow(clippy::too_many_lines)]
async fn load(pool: &MySqlPool, company_id: i32, ids: &[i32]) -> Result<Vec<JobChips>, ApiError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut query = QueryBuilder::<MySql>::new(
        "SELECT e.id, e.sale_id, CAST(e.customer_id AS SIGNED) AS customer_id, e.location
           FROM events e JOIN users u ON e.created_user_id = u.id
          WHERE e.deleted_date IS NULL AND u.company_id = ",
    );
    query.push_bind(company_id);
    crate::sql::push_in(&mut query, "e.id", ids);
    query.push(" ORDER BY e.id");
    let events: Vec<EventRow> = query.build_query_as().fetch_all(pool).await?;

    // Each job's project, as `GET /v1/events/{id}/sale` finds it; the card
    // falls back to the event's own sale when that lookup fails.
    let mut projects: Vec<Vec<i32>> = Vec::with_capacity(events.len());
    for event in &events {
        let sales = if event.sale_id.is_some() || event.customer_id.is_some() {
            find_event_sales(pool, company_id, event.id)
                .await
                .unwrap_or_else(|_| event.sale_id.into_iter().collect())
        } else {
            Vec::new()
        };
        projects.push(sales);
    }

    let sale_ids: Vec<i32> = {
        let mut all: Vec<i32> = projects.iter().flatten().copied().collect();
        all.sort_unstable();
        all.dedup();
        all
    };
    // A sale the card can read: `GET /v1/sales/{id}` (the customer is the
    // company's, a slab is left) and `GET /v1/transactions/{id}` (the sale is
    // the company's, its seller exists) both answer.
    let readable: HashSet<i32> = if sale_ids.is_empty() {
        HashSet::new()
    } else {
        let mut query = in_list(
            "SELECT s.id FROM sales s
               JOIN customers c ON c.id = s.customer_id
               JOIN users u ON u.id = s.seller_id
              WHERE EXISTS (SELECT 1 FROM slab_inventory si WHERE si.sale_id = s.id)",
            "s.id",
            &sale_ids,
        );
        query.push(" AND c.company_id = ");
        query.push_bind(company_id);
        query.push(" AND s.company_id = ");
        query.push_bind(company_id);
        query
            .build_query_scalar::<i32>()
            .fetch_all(pool)
            .await?
            .into_iter()
            .collect()
    };
    let slabs: Vec<SlabRow> = if sale_ids.is_empty() {
        Vec::new()
    } else {
        let mut query = in_list(
            "SELECT si.sale_id, st.name AS stone_name, CAST(si.parent_id AS SIGNED) AS parent_id
               FROM slab_inventory si JOIN stones st ON si.stone_id = st.id
              WHERE 1 = 1",
            "si.sale_id",
            &sale_ids,
        );
        query.push(" ORDER BY si.id");
        query.build_query_as().fetch_all(pool).await?
    };
    let room_slabs: Vec<RoomSlabRow> = if sale_ids.is_empty() {
        Vec::new()
    } else {
        let mut query = in_list(
            "SELECT slab.sale_id, BIN_TO_UUID(slab.room_uuid) AS room_id,
                    CAST(slab.square_feet AS CHAR) AS square_feet
               FROM slab_inventory slab
              WHERE 1 = 1",
            "slab.sale_id",
            &sale_ids,
        );
        query.push(" ORDER BY slab.id");
        query.build_query_as().fetch_all(pool).await?
    };

    // Each sale's stones and its rooms' sq ft (a room is its first slab's).
    let mut stones_of: HashMap<i32, Vec<String>> = HashMap::new();
    for slab in &slabs {
        if slab.parent_id.is_none() {
            stones_of
                .entry(slab.sale_id)
                .or_default()
                .push(slab.stone_name.clone().unwrap_or_default());
        }
    }
    let mut rooms_of: HashMap<i32, Vec<(Option<&str>, f64)>> = HashMap::new();
    for slab in &room_slabs {
        let rooms = rooms_of.entry(slab.sale_id).or_default();
        let room_id = slab.room_id.as_deref();
        if !rooms.iter().any(|(id, _)| *id == room_id) {
            rooms.push((room_id, js_number(slab.square_feet.as_deref())));
        }
    }

    // The customers' quotes, for the sold quotes at each job's address.
    let customers: Vec<i64> = {
        let mut all: Vec<i64> = events
            .iter()
            .filter_map(|event| event.customer_id.filter(|id| *id > 0))
            .collect();
        all.sort_unstable();
        all.dedup();
        all
    };
    let quotes: Vec<QuoteRow> = if customers.is_empty() {
        Vec::new()
    } else {
        let mut query = QueryBuilder::<MySql>::new(
            "SELECT q.id, CAST(q.customer_id AS SIGNED) AS customer_id, q.status, q.address
               FROM quotes q
              WHERE q.deleted_at IS NULL AND q.company_id = ",
        );
        query.push_bind(company_id);
        crate::sql::push_in(&mut query, "q.customer_id", &customers);
        query.build_query_as().fetch_all(pool).await?
    };
    let shown: Vec<Vec<i32>> = events
        .iter()
        .map(|event| {
            let Some(customer) = event.customer_id.filter(|id| *id > 0) else {
                return Vec::new();
            };
            let candidates: Vec<SoldCandidate<'_>> = quotes
                .iter()
                .filter(|quote| quote.customer_id == customer)
                .map(|quote| SoldCandidate {
                    id: quote.id,
                    status: quote.status.as_deref().unwrap_or(""),
                    address: quote.address.as_deref(),
                })
                .collect();
            drawing_quotes(&candidates, event.location.as_deref())
        })
        .collect();
    let quote_ids: Vec<i32> = {
        let mut all: Vec<i32> = shown.iter().flatten().copied().collect();
        all.sort_unstable();
        all.dedup();
        all
    };
    let quote_square_feet: HashMap<i32, f64> = quotes_square_feet(pool, company_id, &quote_ids)
        .await?
        .into_iter()
        .map(|quote| (quote.id, quote.square_feet))
        .collect();

    Ok(events
        .iter()
        .zip(&projects)
        .zip(&shown)
        .map(|((event, sales), quotes)| {
            // One sale the card can't read loses the whole project, as on
            // the page.
            let project_read = !sales.is_empty() && sales.iter().all(|id| readable.contains(id));
            let mut stones: Vec<String> = Vec::new();
            let mut rooms_square_feet = 0.0;
            if project_read {
                for sale in sales {
                    for stone in stones_of.get(sale).into_iter().flatten() {
                        if !stone.is_empty() && !stones.contains(stone) {
                            stones.push(stone.clone());
                        }
                    }
                    for (_, square_feet) in rooms_of.get(sale).into_iter().flatten() {
                        rooms_square_feet += square_feet;
                    }
                }
            }
            let from_quotes: f64 = quotes
                .iter()
                .filter_map(|id| quote_square_feet.get(id))
                .sum();
            JobChips {
                event_id: event.id,
                stones,
                square_feet: if from_quotes > 0.0 { from_quotes } else { rooms_square_feet },
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quote<'a>(id: i32, status: &'a str, address: Option<&'a str>) -> SoldCandidate<'a> {
        SoldCandidate { id, status, address }
    }

    #[test]
    fn sold_quotes_at_the_events_street() {
        let quotes = [
            quote(1, "accepted", Some("647 Overpeck Rd, Carmel")),
            quote(2, "converted", Some("647 N Overpeck Road")),
            quote(3, "draft", Some("647 Overpeck Rd")),
            quote(4, "accepted", Some("12 Main St")),
        ];
        assert_eq!(drawing_quotes(&quotes, Some("647 Overpeck Road, IN")), [1, 2]);
    }

    #[test]
    fn another_street_shows_none_when_the_customer_has_several_jobs() {
        let quotes = [
            quote(1, "accepted", Some("647 Overpeck Rd")),
            quote(2, "accepted", Some("12 Main St")),
        ];
        assert_eq!(drawing_quotes(&quotes, Some("99 Elm St")), [] as [i32; 0]);
        assert_eq!(drawing_quotes(&quotes, None), [] as [i32; 0]);
    }

    #[test]
    fn one_job_lends_its_quotes_only_without_an_event_street() {
        let quotes = [
            quote(1, "accepted", Some("647 Overpeck Rd")),
            quote(2, "converted", Some("647 Overpeck Road")),
        ];
        assert_eq!(drawing_quotes(&quotes, None), [1, 2]);
        assert_eq!(drawing_quotes(&quotes, Some("Carmel, IN")), [1, 2]);
        assert_eq!(drawing_quotes(&quotes, Some("99 Elm St")), [] as [i32; 0]);
    }

    #[test]
    fn quotes_without_a_street_count_only_alone() {
        let one = [quote(1, "accepted", None), quote(2, "draft", None)];
        assert_eq!(drawing_quotes(&one, Some("99 Elm St")), [1]);
        let two = [quote(1, "accepted", None), quote(2, "accepted", Some("Carmel"))];
        assert_eq!(drawing_quotes(&two, None), [] as [i32; 0]);
    }

    #[test]
    fn nothing_sold_shows_nothing() {
        let quotes = [quote(1, "draft", Some("647 Overpeck Rd"))];
        assert_eq!(drawing_quotes(&quotes, Some("647 Overpeck Rd")), [] as [i32; 0]);
        assert_eq!(drawing_quotes(&[], None), [] as [i32; 0]);
    }

    #[test]
    fn numbers_read_like_javascript() {
        assert!((js_number(Some("12.50")) - 12.5).abs() < f64::EPSILON);
        assert!((js_number(Some(" 7 ")) - 7.0).abs() < f64::EPSILON);
        assert!(js_number(Some("abc")).abs() < f64::EPSILON);
        assert!(js_number(Some("")).abs() < f64::EPSILON);
        assert!(js_number(None).abs() < f64::EPSILON);
    }
}
