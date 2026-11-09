//! The schedule settings: the company's (Admin > Schedule) and each user's
//! own (the schedule's settings button).
//!
//! - `GET /v1/schedule/settings` → the day the caller's weeks start on, their
//!   12- or 24-hour times, their maps app, the calendar the Schedule estimate
//!   button books on and the label color they use most
//! - `PUT /v1/schedule/settings` → an admin changes the company's
//! - `PUT /v1/schedule/settings/me` → a user changes their own

use crate::auth::{AdminUser, CurrentUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;

/// The first day of a week.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum WeekStart {
    Sunday,
    Monday,
}

/// The maps app a user's addresses and directions open in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum MapsApp {
    /// Google Maps.
    Google,
    /// Apple Maps.
    Apple,
}

impl MapsApp {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Google => "google",
            Self::Apple => "apple",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "google" => Some(Self::Google),
            "apple" => Some(Self::Apple),
            _ => None,
        }
    }
}

/// `GET /v1/schedule/settings`.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ScheduleSettings {
    /// The day the caller's week and month views start their weeks on: their
    /// own choice, else the company's.
    pub week_starts_on: WeekStart,
    /// The day the company's weeks start on (Admin > Schedule).
    pub company_week_starts_on: WeekStart,
    /// The caller shows times as 13:00 rather than 1:00 PM.
    pub time_24h: bool,
    /// The maps app the caller's addresses and directions open in; null when
    /// they have not picked one (the device's own: Apple Maps on Apple
    /// devices).
    pub maps_app: Option<MapsApp>,
    /// The Schedule already asked the caller for their 12/24-hour times and
    /// maps app (answered or closed): it does not ask again.
    pub prefs_asked: bool,
    /// The calendar the deal page's Schedule estimate button books on: the
    /// one an admin picked, else the built-in Estimates calendar (null when
    /// there is neither).
    pub estimates_calendar_id: Option<i32>,
    /// The label color the caller gave most of the events they added in the
    /// last 90 days (null when none): a new event starts with it.
    pub favorite_color: Option<String>,
    /// The job's sales rep is told when the installer taps Completed on an
    /// installation (Admin > Schedule; on unless an admin turned it off).
    pub notify_rep_on_install_complete: bool,
}

/// `PUT /v1/schedule/settings`: only the fields sent change.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ScheduleSettingsInput {
    /// The day the week and month views start their weeks on.
    #[serde(default)]
    pub week_starts_on: Option<WeekStart>,
    /// A calendar of the caller's company.
    #[serde(default)]
    pub estimates_calendar_id: Option<i32>,
    /// Tell the sales rep when the installer taps Completed on an installation.
    #[serde(default)]
    pub notify_rep_on_install_complete: Option<bool>,
}

/// The calendar estimates are booked on: the one an admin picked while it
/// exists, else the built-in Estimates calendar.
pub async fn estimates_calendar_id(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT cal.id FROM calendars cal JOIN company co ON co.id = cal.company_id
          WHERE cal.company_id = ? AND cal.deleted_at IS NULL
            AND (cal.id = co.estimates_calendar_id OR cal.slug = 'estimates')
          ORDER BY COALESCE(cal.id = co.estimates_calendar_id, 0) DESC, cal.id
          LIMIT 1",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await
}

/// The color the user gave most of the events they added in the last 90
/// days; the latest wins a tie.
async fn favorite_color(pool: &MySqlPool, user_id: i32) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT color FROM events
          WHERE created_user_id = ? AND created_date >= NOW() - INTERVAL 90 DAY
            AND COALESCE(TRIM(color), '') <> ''
          GROUP BY color ORDER BY COUNT(*) DESC, MAX(id) DESC LIMIT 1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await
}

/// The caller's maps app and whether the Schedule asked them yet. Before the
/// columns exist (the API deployed ahead of its migration) it reads as no
/// maps app, asked: nobody is asked a question whose answer cannot be saved.
async fn maps_and_asked(pool: &MySqlPool, user_id: i32) -> (Option<MapsApp>, bool) {
    let row: Result<Option<(Option<String>, bool)>, sqlx::Error> = sqlx::query_as(
        "SELECT maps_app, schedule_prefs_asked_at IS NOT NULL FROM users WHERE id = ?",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await;
    match row {
        Ok(Some((maps_app, asked))) => (maps_app.as_deref().and_then(MapsApp::parse), asked),
        Ok(None) => (None, true),
        Err(error) => {
            tracing::warn!(?error, user_id, "schedule settings: maps app not read");
            (None, true)
        }
    }
}

/// Get the schedule settings
///
/// The day the caller's schedule starts its weeks on (their own choice, else
/// the company's: Sunday unless an admin picked Monday), whether they show
/// 24-hour times, the maps app their addresses open in (null: the device's
/// own), whether the Schedule already asked them for those two, the calendar estimates are booked on, and the label color
/// they use most.
#[crate::openapi::path(
    get,
    path = "/v1/schedule/settings",
    operation_id = "get_schedule_settings",
    tag = "schedule",
    responses(
        (status = 200, description = "Settings", body = ScheduleSettings),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn settings(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<Json<ScheduleSettings>> {
    let company: Option<(bool, bool)> = sqlx::query_as(
        "SELECT schedule_week_starts_monday <> 0, notify_rep_on_install_complete <> 0
           FROM company WHERE id = ?",
    )
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?;
    let (company_monday, notify_rep_on_install_complete) = company.unwrap_or((false, true));
    let own: Option<(Option<bool>, bool)> = sqlx::query_as(
        "SELECT schedule_week_starts_monday <> 0, schedule_time_24h <> 0 FROM users WHERE id = ?",
    )
    .bind(user.id)
    .fetch_optional(&state.pool)
    .await?;
    let (own_monday, time_24h) = own.unwrap_or((None, false));
    let (maps_app, prefs_asked) = maps_and_asked(&state.pool, user.id).await;
    let week_start = |monday: bool| {
        if monday {
            WeekStart::Monday
        } else {
            WeekStart::Sunday
        }
    };
    Ok(Json(ScheduleSettings {
        week_starts_on: week_start(own_monday.unwrap_or(company_monday)),
        company_week_starts_on: week_start(company_monday),
        time_24h,
        maps_app,
        prefs_asked,
        estimates_calendar_id: estimates_calendar_id(&state.pool, user.company_id).await?,
        favorite_color: favorite_color(&state.pool, user.id).await?,
        notify_rep_on_install_complete,
    }))
}

/// Set the schedule settings
///
/// Saves the day the company's schedule starts its weeks on, the calendar
/// estimates are booked on, and whether the sales rep is told when the
/// installer taps Completed on an installation, for everyone.
#[crate::openapi::path(
    put,
    path = "/v1/schedule/settings",
    operation_id = "set_schedule_settings",
    tag = "schedule",
    request_body = ScheduleSettingsInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Not `sunday` or `monday`, or not a calendar of the company", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_settings(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<ScheduleSettingsInput>,
) -> ApiResult<StatusCode> {
    if let Some(calendar_id) = input.estimates_calendar_id {
        let found: Option<i32> = sqlx::query_scalar(
            "SELECT id FROM calendars WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
        )
        .bind(calendar_id)
        .bind(user.company_id)
        .fetch_optional(&state.pool)
        .await?;
        if found.is_none() {
            return Err(ApiError::BadRequest("Calendar not found".into()));
        }
        sqlx::query("UPDATE company SET estimates_calendar_id = ? WHERE id = ?")
            .bind(calendar_id)
            .bind(user.company_id)
            .execute(&state.pool)
            .await?;
    }
    if let Some(on) = input.notify_rep_on_install_complete {
        sqlx::query("UPDATE company SET notify_rep_on_install_complete = ? WHERE id = ?")
            .bind(on)
            .bind(user.company_id)
            .execute(&state.pool)
            .await?;
    }
    if let Some(day) = input.week_starts_on {
        sqlx::query("UPDATE company SET schedule_week_starts_monday = ? WHERE id = ?")
            .bind(day == WeekStart::Monday)
            .bind(user.company_id)
            .execute(&state.pool)
            .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /v1/schedule/settings/me`: only the fields sent change.
#[derive(Debug, Deserialize, ToSchema)]
pub struct MyScheduleSettingsInput {
    /// The day the caller's week and month views start their weeks on.
    #[serde(default)]
    pub week_starts_on: Option<WeekStart>,
    /// Show times as 13:00 rather than 1:00 PM.
    #[serde(default)]
    pub time_24h: Option<bool>,
    /// The maps app the caller's addresses and directions open in.
    #[serde(default)]
    pub maps_app: Option<MapsApp>,
    /// `true`: the Schedule asked the caller for their times and maps app
    /// (answered or closed), so it does not ask again. `false` is ignored.
    #[serde(default)]
    pub prefs_asked: Option<bool>,
}

/// Set my schedule settings
///
/// Saves the day the caller's own schedule starts its weeks on, whether it
/// shows 24-hour times, the maps app their addresses open in, and that the
/// Schedule asked them for those (`prefs_asked`). Only the caller sees them.
#[crate::openapi::path(
    put,
    path = "/v1/schedule/settings/me",
    operation_id = "set_my_schedule_settings",
    tag = "schedule",
    request_body = MyScheduleSettingsInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Not `sunday` or `monday`, or not `google` or `apple`", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_my_settings(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    JsonBody(input): JsonBody<MyScheduleSettingsInput>,
) -> ApiResult<StatusCode> {
    if let Some(day) = input.week_starts_on {
        sqlx::query("UPDATE users SET schedule_week_starts_monday = ? WHERE id = ?")
            .bind(day == WeekStart::Monday)
            .bind(user.id)
            .execute(&state.pool)
            .await?;
    }
    if let Some(time_24h) = input.time_24h {
        sqlx::query("UPDATE users SET schedule_time_24h = ? WHERE id = ?")
            .bind(time_24h)
            .bind(user.id)
            .execute(&state.pool)
            .await?;
    }
    if let Some(maps_app) = input.maps_app {
        sqlx::query("UPDATE users SET maps_app = ? WHERE id = ?")
            .bind(maps_app.as_str())
            .bind(user.id)
            .execute(&state.pool)
            .await?;
    }
    if input.prefs_asked == Some(true) {
        sqlx::query(
            "UPDATE users SET schedule_prefs_asked_at = COALESCE(schedule_prefs_asked_at, UTC_TIMESTAMP())
              WHERE id = ?",
        )
        .bind(user.id)
        .execute(&state.pool)
        .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}
