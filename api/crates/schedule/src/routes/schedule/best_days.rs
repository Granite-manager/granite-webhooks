//! The best days for a waiting job, by route.
//!
//! - `POST /v1/schedule/best-days` → how far a waiting job is from each
//!   later stop of its calendar, so the month can light up the days whose
//!   route passes closest while the job is dragged (Best days while dragging
//!   skill). Template and installation jobs.

use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Most later stops looked at in one ask.
const MAX_STOPS: usize = 80;

/// Body of `POST /v1/schedule/best-days`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct BestDaysInput {
    /// The waiting job being dragged.
    pub event_id: i32,
    /// Range start (ISO 8601); nothing before now is looked at.
    pub start: DateTime<Utc>,
    /// Range end (ISO 8601).
    pub end: DateTime<Utc>,
}

/// A later stop and how far the job is from it.
#[derive(Debug, Serialize, ToSchema)]
pub struct StopDistance {
    /// The stop's event id.
    pub event_id: i32,
    /// Miles as the crow flies, one decimal.
    pub miles: f64,
    /// Driving minutes from the job to the stop (Google), whole minutes;
    /// `null` when Google has no route or fails.
    pub minutes: Option<u32>,
}

/// `POST /v1/schedule/best-days`.
#[derive(Debug, Serialize, ToSchema)]
pub struct BestDays {
    /// The job's calendar's events of the range that start after now and
    /// have an address Google can find. Empty when the job has no address,
    /// Google cannot find it, or Google Maps is not set up.
    pub stops: Vec<StopDistance>,
}

#[derive(sqlx::FromRow)]
struct Stop {
    id: i32,
    location: String,
}

/// Best days for a waiting job
///
/// Miles and driving minutes from the job to every one-off event of its
/// calendar with an address between now (or the range start, if later) and
/// the range end. The app lights up the closest events while the job is
/// dragged. Template and installation jobs; any other job gets no stops.
#[crate::openapi::path(
    post,
    path = "/v1/schedule/best-days",
    operation_id = "schedule_best_days",
    tag = "schedule",
    request_body = BestDaysInput,
    responses(
        (status = 200, description = "Distances", body = BestDays),
        (status = 400, description = "The range ends before it starts", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin, or lacks the Best days while dragging skill", body = ErrorBody),
        (status = 404, description = "No such event in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn best_days(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<BestDaysInput>,
) -> ApiResult<Json<BestDays>> {
    if input.end < input.start {
        return Err(ApiError::BadRequest(
            "The range ends before it starts".into(),
        ));
    }
    let pool = &state.pool;
    let job: Option<(Option<String>, Option<i32>, Option<String>)> = sqlx::query_as(
        "SELECT NULLIF(TRIM(e.location), ''), e.calendar_id, c.slug
           FROM events e JOIN users u ON e.created_user_id = u.id
           LEFT JOIN calendars c ON c.id = e.calendar_id AND c.company_id = u.company_id
          WHERE e.id = ? AND u.company_id = ? AND e.deleted_date IS NULL",
    )
    .bind(input.event_id)
    .bind(user.company_id)
    .fetch_optional(pool)
    .await?;
    let (location, calendar_id, slug) = job.ok_or(ApiError::NotFound)?;
    let none = || Json(BestDays { stops: Vec::new() });
    // Template and installation jobs.
    if !matches!(slug.as_deref(), Some("templates" | "installations")) {
        return Ok(none());
    }
    let Some(location) = location else {
        return Ok(none());
    };
    let start = input.start.max(Utc::now()).naive_utc();
    let end = input.end.naive_utc();
    if end < start {
        return Ok(none());
    }
    let stops: Vec<Stop> = sqlx::query_as(
        "SELECT e.id, TRIM(e.location) AS location
           FROM events e JOIN users u ON e.created_user_id = u.id
          WHERE u.company_id = ? AND e.deleted_date IS NULL AND e.unscheduled = 0
            AND e.id <> ? AND e.calendar_id <=> ?
            AND NULLIF(TRIM(e.rrule), '') IS NULL
            AND e.location IS NOT NULL AND TRIM(e.location) <> ''
            AND e.start_date >= ? AND e.start_date <= ?
          ORDER BY e.start_date, e.id
          LIMIT ?",
    )
    .bind(user.company_id)
    .bind(input.event_id)
    .bind(calendar_id)
    .bind(start)
    .bind(end)
    .bind(MAX_STOPS as i64)
    .fetch_all(pool)
    .await?;
    if stops.is_empty() {
        return Ok(none());
    }
    // Each address once (whatever its case): the job's first, then the stops'.
    let mut addresses: Vec<String> = vec![location];
    for stop in &stops {
        if !addresses
            .iter()
            .any(|a| a.eq_ignore_ascii_case(&stop.location))
        {
            addresses.push(stop.location.clone());
        }
    }
    let points = state.maps.geocode_all(&addresses).await;
    let Some(job_point) = points[0] else {
        return Ok(none());
    };
    // One driving-time ask from the job to every found stop address.
    let found: Vec<usize> = (1..addresses.len())
        .filter(|&at| points[at].is_some())
        .collect();
    let targets: Vec<_> = found.iter().filter_map(|&at| points[at]).collect();
    let drive = state.maps.drive_minutes_from(job_point, &targets).await;
    let minutes_at = |at: usize| {
        let index = found.iter().position(|&f| f == at)?;
        drive[index].map(|minutes| minutes.round() as u32)
    };
    let stops = stops
        .iter()
        .filter_map(|stop| {
            let at = addresses
                .iter()
                .position(|a| a.eq_ignore_ascii_case(&stop.location))?;
            let point = points[at]?;
            Some(StopDistance {
                event_id: stop.id,
                miles: (job_point.miles_to(point) * 10.0).round() / 10.0,
                minutes: minutes_at(at),
            })
        })
        .collect();
    Ok(Json(BestDays { stops }))
}
