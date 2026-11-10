//! Admin > Quotes > Automation > Event templates: extra events a quote's sale
//! can add to the schedule.
//!
//! Each template names a calendar and a title made of a variable (the quote's
//! or the customer's name) and the text after it. Selling "Ostrom Kitchen"
//! with a "Quote name" + "Plumbing" template ticked in Mark as Sold › Extra
//! events (`extra_events` of `POST /v1/quotes/{id}/sale`) adds an unscheduled
//! "Ostrom Kitchen - Plumbing" event to that calendar
//! (`quote_pricing::add_waiting_jobs`). Undo sale deletes it with the sale's
//! other jobs.
//!
//! - `GET /v1/quote-sale-events` (employees: the Mark as Sold checkboxes)
//! - `POST /v1/quote-sale-events`, `PUT|DELETE /v1/quote-sale-events/{id}` (admins)

use crate::auth::{AdminUser, EmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, MySqlPool};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_sale_events, create_sale_event))
        .routes(routes!(update_sale_event, delete_sale_event))
        .routes(routes!(get_built_in_titles, save_built_in_titles))
}

/// Longest text before or after the variable.
const MAX_TEXT: usize = 200;

/// What an event template's title starts with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TitleVariable {
    /// The quote's name; a quote without one uses the customer's name.
    #[default]
    QuoteName,
    /// The customer's name; a customer without one uses the quote's name.
    CustomerName,
}

impl TitleVariable {
    const fn as_str(self) -> &'static str {
        match self {
            Self::QuoteName => "quote_name",
            Self::CustomerName => "customer_name",
        }
    }

    fn parse(text: &str) -> Self {
        if text == "customer_name" {
            Self::CustomerName
        } else {
            Self::QuoteName
        }
    }
}

/// An extra event a quote's sale can add to the schedule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct QuoteSaleEvent {
    /// Template id.
    pub id: i32,
    /// The calendar the event goes on.
    pub calendar_id: i32,
    /// That calendar's name.
    pub calendar_name: String,
    /// The name the event's title is built around.
    pub title_variable: TitleVariable,
    /// What comes before the variable, e.g. "Rough-in"; empty: nothing.
    pub title_before: String,
    /// What follows the variable, e.g. "Plumbing"; empty: the variable alone.
    pub title_text: String,
    /// Catalog extras (`GET /v1/quote-extras` ids) that tick this event in
    /// Mark as Sold when the quote has one of them (a line whose `catalogId`
    /// is one of these, any option). Empty: the seller ticks it by hand.
    pub trigger_extra_ids: Vec<i64>,
    /// Single options of catalog extras that tick this event in Mark as Sold
    /// when the quote has a line of that extra with that option (its
    /// `catalogId` and `optionId`).
    pub trigger_options: Vec<TriggerOption>,
}

/// One option of a catalog extra (`arrOptions[].id` of a `GET /v1/quote-extras` item).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TriggerOption {
    /// The catalog extra.
    pub extra_id: i64,
    /// The option's id in that extra.
    pub option_id: String,
}

#[derive(FromRow)]
struct SaleEventRow {
    id: i32,
    calendar_id: i32,
    calendar_name: String,
    title_variable: String,
    title_before: String,
    title_text: String,
    trigger_extra_ids: Option<String>,
    trigger_options: Option<String>,
}

impl From<SaleEventRow> for QuoteSaleEvent {
    fn from(row: SaleEventRow) -> Self {
        Self {
            id: row.id,
            calendar_id: row.calendar_id,
            calendar_name: row.calendar_name,
            title_variable: TitleVariable::parse(&row.title_variable),
            title_before: row.title_before,
            title_text: row.title_text,
            trigger_extra_ids: row
                .trigger_extra_ids
                .and_then(|ids| serde_json::from_str(&ids).ok())
                .unwrap_or_default(),
            trigger_options: row
                .trigger_options
                .and_then(|options| serde_json::from_str(&options).ok())
                .unwrap_or_default(),
        }
    }
}

/// Body of creating or saving an event template.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuoteSaleEventInput {
    /// A calendar of the company (`GET /v1/calendars`).
    pub calendar_id: i32,
    /// What the title starts with (default `quote_name`).
    #[serde(default)]
    pub title_variable: TitleVariable,
    /// What comes before the variable, e.g. "Rough-in" (at most 200
    /// characters; blank: nothing).
    #[serde(default)]
    pub title_before: String,
    /// What follows the variable, e.g. "Plumbing" (at most 200 characters;
    /// blank: the variable alone).
    #[serde(default)]
    pub title_text: String,
    /// Catalog extras (`GET /v1/quote-extras` ids) that tick this event in
    /// Mark as Sold when the quote has one. Ids not in the company's catalog
    /// are dropped. Default none.
    #[serde(default)]
    pub trigger_extra_ids: Vec<i64>,
    /// Single options of catalog extras that tick this event in Mark as Sold
    /// when the quote has one. Options not in the company's catalog are
    /// dropped. Default none.
    #[serde(default)]
    pub trigger_options: Vec<TriggerOption>,
}

/// `POST /v1/quote-sale-events`.
#[derive(Debug, Serialize, ToSchema)]
pub struct CreatedSaleEvent {
    /// The new template.
    pub id: u64,
}

/// The title a template gives a sold quote's event: the text before, the
/// variable's name and the text after, joined by " - " (blank parts left
/// out). `quote_name` and `customer_name` are trimmed and `None` when blank;
/// `fallback` stands in when both are.
#[must_use]
pub fn sale_event_title(
    variable: TitleVariable,
    before: &str,
    text: &str,
    quote_name: Option<&str>,
    customer_name: Option<&str>,
    fallback: &str,
) -> String {
    let (first, second) = match variable {
        TitleVariable::QuoteName => (quote_name, customer_name),
        TitleVariable::CustomerName => (customer_name, quote_name),
    };
    let name = first.or(second).unwrap_or(fallback);
    let title = [before.trim(), name, text.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" - ");
    title.chars().take(255).collect()
}

/// How a sold quote's built-in template or installation event is named:
/// the text before, the variable, the text after.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SaleEventName {
    /// The name the title is built around (default `quote_name`).
    #[serde(default)]
    pub title_variable: TitleVariable,
    /// What comes before the variable (at most 200 characters; blank: nothing).
    #[serde(default)]
    pub title_before: String,
    /// What follows the variable (at most 200 characters; blank: nothing).
    #[serde(default)]
    pub title_text: String,
}

impl SaleEventName {
    /// The title for a sold quote (see `sale_event_title`).
    #[must_use]
    pub fn title(
        &self,
        quote_name: Option<&str>,
        customer_name: Option<&str>,
        fallback: &str,
    ) -> String {
        sale_event_title(
            self.title_variable,
            &self.title_before,
            &self.title_text,
            quote_name,
            customer_name,
            fallback,
        )
    }
}

/// `GET|PUT /v1/quote-sale-built-in-titles`: how the company names the
/// template and installation events a sale adds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BuiltInTitles {
    /// The template event (default: the quote's name alone).
    pub template: SaleEventName,
    /// The installation event (default: the quote's name alone).
    pub installation: SaleEventName,
}

/// The company's built-in event names (`company.sale_template_title` and
/// `sale_installation_title`; unset: the quote's name alone).
pub async fn built_in_titles(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<BuiltInTitles, sqlx::Error> {
    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT CAST(sale_template_title AS CHAR), CAST(sale_installation_title AS CHAR)
           FROM company WHERE id = ?",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let parse = |json: Option<String>| {
        json.and_then(|json| serde_json::from_str::<SaleEventName>(&json).ok())
            .unwrap_or_default()
    };
    let (template, installation) = row.unwrap_or_default();
    Ok(BuiltInTitles {
        template: parse(template),
        installation: parse(installation),
    })
}

/// The text trimmed; 400 when longer than `MAX_TEXT`.
fn checked_text(text: &str, side: &str) -> ApiResult<String> {
    let text = text.trim().to_string();
    if text.chars().count() > MAX_TEXT {
        return Err(ApiError::BadRequest(format!(
            "The text {side} the variable can be at most {MAX_TEXT} characters"
        )));
    }
    Ok(text)
}

/// The company's event templates on calendars it still has, in the order they
/// were added.
pub async fn sale_events(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<Vec<QuoteSaleEvent>, sqlx::Error> {
    let rows: Vec<SaleEventRow> = sqlx::query_as(
        "SELECT t.id, t.calendar_id, c.name AS calendar_name,
                CAST(t.title_variable AS CHAR) AS title_variable, t.title_before,
                t.title_text,
                CAST(t.trigger_extra_ids AS CHAR) AS trigger_extra_ids,
                CAST(t.trigger_options AS CHAR) AS trigger_options
           FROM quote_sale_event_templates t
           JOIN calendars c ON c.id = t.calendar_id AND c.company_id = t.company_id
                           AND c.deleted_at IS NULL
          WHERE t.company_id = ?
          ORDER BY t.id",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(QuoteSaleEvent::from).collect())
}

/// What `check` accepted: the texts trimmed, the trigger extras and options as JSON.
struct Checked {
    before: String,
    text: String,
    triggers: String,
    options: String,
}

/// The `arrOptions[].id`s of a catalog extra's `item_json`.
fn option_ids(item_json: &str) -> Vec<String> {
    serde_json::from_str::<serde_json::Value>(item_json)
        .ok()
        .and_then(|item| item.get("arrOptions").and_then(|o| o.as_array()).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|option| {
            option
                .get("id")
                .and_then(|id| id.as_str())
                .map(str::to_string)
        })
        .collect()
}

/// The trigger options whose extra is in `catalog` and has that option, once each.
fn kept_options(wanted: &[TriggerOption], catalog: &[(i64, String)]) -> Vec<TriggerOption> {
    let mut kept: Vec<TriggerOption> = wanted
        .iter()
        .filter(|want| {
            catalog.iter().any(|(id, item)| {
                *id == want.extra_id && option_ids(item).contains(&want.option_id)
            })
        })
        .cloned()
        .collect();
    kept.sort();
    kept.dedup();
    kept
}

/// The text trimmed and checked; the calendar the company's; the trigger
/// extras kept when in the company's catalog, once each.
async fn check(
    pool: &MySqlPool,
    company_id: i32,
    input: &QuoteSaleEventInput,
) -> ApiResult<Checked> {
    let before = checked_text(&input.title_before, "before")?;
    let text = checked_text(&input.title_text, "after")?;
    let calendar: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM calendars WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(input.calendar_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    if calendar.is_none() {
        return Err(ApiError::BadRequest("Choose a calendar".into()));
    }
    let catalog: Vec<(i64, String)> = sqlx::query_as(
        "SELECT CAST(id AS SIGNED), CAST(item_json AS CHAR) FROM quote_extras
          WHERE company_id = ? AND deleted_at IS NULL",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    let mut triggers: Vec<i64> = input
        .trigger_extra_ids
        .iter()
        .copied()
        .filter(|id| catalog.iter().any(|(extra, _)| extra == id))
        .collect();
    triggers.sort_unstable();
    triggers.dedup();
    let options = kept_options(&input.trigger_options, &catalog);
    Ok(Checked {
        before,
        text,
        triggers: serde_json::to_string(&triggers).unwrap_or_else(|_| "[]".into()),
        options: serde_json::to_string(&options).unwrap_or_else(|_| "[]".into()),
    })
}

/// 404 unless template `id` is the company's.
async fn find(pool: &MySqlPool, company_id: i32, id: i32) -> ApiResult<()> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM quote_sale_event_templates WHERE id = ? AND company_id = ?",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    found.map(|_| ()).ok_or(ApiError::NotFound)
}

/// List the extra events a quote's sale can add to the schedule
///
/// Returns the company's event templates (Admin > Quotes > Automation), in
/// the order they were added: the checkboxes under Mark as Sold › Extra
/// events. One starts ticked when the quote has a line of one of its
/// `trigger_extra_ids` extras, or one of its `trigger_options`. Pass the ticked ones as `extra_events` to
/// `POST /v1/quotes/{id}/sale`. A template whose calendar was deleted is left
/// out. Any employee may read them; admins change them with
/// `POST /v1/quote-sale-events` and `PUT|DELETE /v1/quote-sale-events/{id}`.
#[crate::openapi::path(
    get,
    path = "/v1/quote-sale-events",
    operation_id = "list_quote_sale_events",
    tag = "quotes",
    responses(
        (status = 200, description = "The company's event templates", body = Vec<QuoteSaleEvent>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_sale_events(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Vec<QuoteSaleEvent>>> {
    Ok(Json(sale_events(&state.pool, user.company_id).await?))
}

/// Add an extra event a quote's sale can add to the schedule
///
/// From then on, Mark as Sold › Extra events offers it as a checkbox; ticked,
/// the sale adds one unscheduled, Not ready event to `calendar_id`, titled
/// with the variable and the text (e.g. "Ostrom Kitchen - Plumbing"), at the
/// customer's address, for the schedule manager to give a day. Quotes sold
/// before are not changed.
#[crate::openapi::path(
    post,
    path = "/v1/quote-sale-events",
    operation_id = "create_quote_sale_event",
    tag = "quotes",
    request_body = QuoteSaleEventInput,
    responses(
        (status = 201, description = "Added", body = CreatedSaleEvent),
        (status = 400, description = "Text too long, or no such calendar in the company", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_sale_event(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<QuoteSaleEventInput>,
) -> ApiResult<(StatusCode, Json<CreatedSaleEvent>)> {
    let checked = check(&state.pool, user.company_id, &input).await?;
    let id = sqlx::query(
        "INSERT INTO quote_sale_event_templates
           (company_id, calendar_id, title_variable, title_before, title_text,
            trigger_extra_ids, trigger_options, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(user.company_id)
    .bind(input.calendar_id)
    .bind(input.title_variable.as_str())
    .bind(checked.before)
    .bind(checked.text)
    .bind(checked.triggers)
    .bind(checked.options)
    .bind(user.id)
    .execute(&state.pool)
    .await?
    .last_insert_id();
    Ok((StatusCode::CREATED, Json(CreatedSaleEvent { id })))
}

/// Edit an extra event a quote's sale can add to the schedule
///
/// Quotes sold from then on get the new calendar, title and trigger extras;
/// events already added stay as they are.
#[crate::openapi::path(
    put,
    path = "/v1/quote-sale-events/{id}",
    operation_id = "update_quote_sale_event",
    tag = "quotes",
    params(("id" = i32, Path, description = "Event template id")),
    request_body = QuoteSaleEventInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Text too long, or no such calendar in the company", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such template in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_sale_event(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<QuoteSaleEventInput>,
) -> ApiResult<StatusCode> {
    find(&state.pool, user.company_id, id).await?;
    let checked = check(&state.pool, user.company_id, &input).await?;
    sqlx::query(
        "UPDATE quote_sale_event_templates
            SET calendar_id = ?, title_variable = ?, title_before = ?, title_text = ?,
                trigger_extra_ids = ?, trigger_options = ?
          WHERE id = ? AND company_id = ?",
    )
    .bind(input.calendar_id)
    .bind(input.title_variable.as_str())
    .bind(checked.before)
    .bind(checked.text)
    .bind(checked.triggers)
    .bind(checked.options)
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete an extra event a quote's sale can add to the schedule
///
/// Mark as Sold no longer offers it; events already added stay.
#[crate::openapi::path(
    delete,
    path = "/v1/quote-sale-events/{id}",
    operation_id = "delete_quote_sale_event",
    tag = "quotes",
    params(("id" = i32, Path, description = "Event template id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such template in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_sale_event(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    find(&state.pool, user.company_id, id).await?;
    sqlx::query("DELETE FROM quote_sale_event_templates WHERE id = ? AND company_id = ?")
        .bind(id)
        .bind(user.company_id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// How the sale's template and installation events are named
///
/// Admin › Quotes › Automation › Event templates: each built-in event's title
/// is the text before, the variable and the text after, joined by " - "
/// (default: the quote's name alone). Any employee may read them (Mark as
/// Sold shows the titles); admins change them with `PUT`.
#[crate::openapi::path(
    get,
    path = "/v1/quote-sale-built-in-titles",
    operation_id = "get_quote_sale_built_in_titles",
    tag = "quotes",
    responses(
        (status = 200, description = "The company's built-in event names", body = BuiltInTitles),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_built_in_titles(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<BuiltInTitles>> {
    Ok(Json(built_in_titles(&state.pool, user.company_id).await?))
}

/// Rename the sale's template and installation events
///
/// Quotes sold from then on get the new titles; events already added stay
/// as they are.
#[crate::openapi::path(
    put,
    path = "/v1/quote-sale-built-in-titles",
    operation_id = "save_quote_sale_built_in_titles",
    tag = "quotes",
    request_body = BuiltInTitles,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "A text is too long", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn save_built_in_titles(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<BuiltInTitles>,
) -> ApiResult<StatusCode> {
    let checked = |name: &SaleEventName| -> ApiResult<String> {
        let name = SaleEventName {
            title_variable: name.title_variable,
            title_before: checked_text(&name.title_before, "before")?,
            title_text: checked_text(&name.title_text, "after")?,
        };
        Ok(serde_json::to_string(&name).unwrap_or_else(|_| "{}".into()))
    };
    sqlx::query(
        "UPDATE company SET sale_template_title = ?, sale_installation_title = ? WHERE id = ?",
    )
    .bind(checked(&input.template)?)
    .bind(checked(&input.installation)?)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_options_keep_only_catalog_options_once() {
        let catalog = vec![
            (
                3,
                r#"{"name":"Sink","arrOptions":[{"id":"opt-a","name":"Undermount"},{"id":"opt-b","name":"Farm"}]}"#
                    .to_string(),
            ),
            (4, r#"{"name":"Sealer"}"#.to_string()),
        ];
        let want = |extra_id, option_id: &str| TriggerOption {
            extra_id,
            option_id: option_id.into(),
        };
        let kept = kept_options(
            &[
                want(3, "opt-b"),
                want(3, "opt-a"),
                want(3, "opt-b"),
                want(3, "opt-z"),
                want(4, "opt-a"),
                want(9, "opt-a"),
            ],
            &catalog,
        );
        assert_eq!(kept, vec![want(3, "opt-a"), want(3, "opt-b")]);
    }

    #[test]
    fn the_title_is_the_variable_then_the_text() {
        let title = |variable, text, quote, customer| {
            sale_event_title(variable, "", text, quote, customer, "Customer #7")
        };
        // Text before: it leads, joined the same way.
        assert_eq!(
            sale_event_title(
                TitleVariable::QuoteName,
                " Rough-in ",
                "Plumbing",
                Some("Ostrom Kitchen"),
                None,
                "Customer #7"
            ),
            "Rough-in - Ostrom Kitchen - Plumbing"
        );
        assert_eq!(
            sale_event_title(
                TitleVariable::CustomerName,
                "Sink",
                " ",
                None,
                Some("Jo"),
                "x"
            ),
            "Sink - Jo"
        );
        let q = TitleVariable::QuoteName;
        let c = TitleVariable::CustomerName;
        assert_eq!(
            title(q, " Plumbing ", Some("Ostrom Kitchen"), Some("Jo")),
            "Ostrom Kitchen - Plumbing"
        );
        assert_eq!(
            title(c, "Plumbing", Some("Ostrom Kitchen"), Some("Jo")),
            "Jo - Plumbing"
        );
        // Blank text: the variable alone; a missing name: the other one, else the fallback.
        assert_eq!(
            title(q, "  ", Some("Ostrom Kitchen"), None),
            "Ostrom Kitchen"
        );
        assert_eq!(title(q, "Sink", None, Some("Jo")), "Jo - Sink");
        assert_eq!(title(c, "Sink", Some("Ostrom"), None), "Ostrom - Sink");
        assert_eq!(title(c, "Sink", None, None), "Customer #7 - Sink");
        assert_eq!(
            title(q, &"x".repeat(200), Some(&"n".repeat(100)), None)
                .chars()
                .count(),
            255
        );
    }
}
