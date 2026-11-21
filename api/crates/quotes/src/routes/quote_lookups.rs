//! Lookups the quote editor makes while a quote is drawn.
//!
//! - `GET /v1/quotes/suggestions` → area names and miscellaneous / text items
//!   the company typed on its recent quotes, to offer them again
//! - `POST /v1/quotes/suggestions/hide` → the x next to a suggestion: the
//!   company is not offered it any more
//! - `GET /v1/quotes/stone-image?color=&product=` → the catalog photo of one
//!   color, to paint the pieces with it in the Color & Edge preview
//!
//! The suggestions are read from the drawings of the company's most recently
//! changed quotes; only the ones taken out with the x are stored
//! (`quote_suggestion_hidden`).

use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::{IntoParams, ToSchema};
use crate::openapi::{OpenApiRouter, routes};
use crate::routes::quote_editor::{StoneImage, stone_images};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(quote_suggestions))
        .routes(routes!(hide_quote_suggestion))
        .routes(routes!(quote_stone_image))
}

/// How many of the company's latest quotes the suggestions are read from.
const RECENT_QUOTES: i64 = 500;
/// A name or item is offered once quotes of this many different customers use
/// it, so names typed for one job ("Unit 6211", "Michael's room") are not.
const MIN_CUSTOMERS: usize = 2;
/// At most this many area names are returned.
const MAX_AREA_NAMES: usize = 60;
/// At most this many items are returned.
const MAX_ITEMS: usize = 150;
/// Longer names and descriptions are not offered again.
const MAX_TEXT: usize = 200;

/// An area name used on earlier quotes.
#[derive(Debug, Clone, Serialize, ToSchema, PartialEq)]
pub struct AreaNameSuggestion {
    /// The name, spelled as on the latest quote that used it (e.g. `Kitchen`).
    pub name: String,
    /// How many of the recent quotes use it.
    pub uses: i64,
    /// How many different customers those quotes are for (a quote with no
    /// customer counts by its deal, else on its own). Always 2 or more.
    pub customers: i64,
    /// When the latest quote using it was last changed (UTC, ISO 8601); null if unknown.
    pub last_used: Option<String>,
}

/// A miscellaneous or text item used on earlier quotes, with the values it
/// had the last time.
#[derive(Debug, Clone, Serialize, ToSchema, PartialEq)]
pub struct ItemSuggestion {
    /// `misc` (a priced line typed by hand) or `text` (a line of text, no price).
    pub kind: String,
    /// The description, spelled as on the latest quote that used it.
    pub description: String,
    /// Price in dollars the last time; null when it had none (always null for `text`).
    pub price: Option<f64>,
    /// Quantity the last time (1 when not set).
    pub quantity: f64,
    /// The line was hidden on the printed quote.
    pub hide_on_quote: bool,
    /// The project discount applied to it.
    pub allow_discount: bool,
    /// Tax applied to it.
    pub taxable: bool,
    /// How many of the recent quotes use it.
    pub uses: i64,
    /// How many different customers those quotes are for (a quote with no
    /// customer counts by its deal, else on its own). Always 2 or more.
    pub customers: i64,
    /// When the latest quote using it was last changed (UTC, ISO 8601); null if unknown.
    pub last_used: Option<String>,
}

/// A suggestion taken out with the x, still not offered.
#[derive(Debug, Clone, Serialize, ToSchema, PartialEq, Eq)]
pub struct HiddenSuggestion {
    /// `area` (an area name), `misc` or `text` (a custom item's description).
    pub kind: String,
    /// The text as it was shown when it was taken out.
    pub text: String,
}

/// `GET /v1/quotes/suggestions`.
#[derive(Debug, Default, Serialize, ToSchema, PartialEq)]
pub struct QuoteSuggestions {
    /// Area names, used by most customers first (ties: most recent first).
    /// Default names such as `Area #2` and hidden ones are left out.
    pub area_names: Vec<AreaNameSuggestion>,
    /// Miscellaneous and text items, used by most customers first (ties: most
    /// recent first). Hidden ones are left out.
    pub items: Vec<ItemSuggestion>,
    /// Suggestions the company took out with the x. The editor also leaves
    /// these out of its own built-in names (`Kitchen`, `Vanity`, ...) and of
    /// the names on the quote being drawn. Compare ignoring case and spaces.
    pub hidden: Vec<HiddenSuggestion>,
}

#[derive(sqlx::FromRow)]
struct DrawingRow {
    id: i32,
    customer_id: Option<i32>,
    deal_id: Option<i64>,
    areas: Option<String>,
    items: Option<String>,
    used_at: Option<String>,
}

/// One recent quote's areas and items.
struct Drawing {
    areas: Value,
    items: Value,
    /// When the quote was last changed (UTC, ISO 8601).
    used_at: Option<String>,
    /// Who the quote is for: its customer, else its deal, else the quote.
    owner: String,
}

/// A hidden suggestion: what was shown, and when it was hidden (UTC, ISO 8601).
#[derive(Debug, Clone)]
struct Hidden {
    text: String,
    hidden_at: String,
}

#[derive(sqlx::FromRow)]
struct HiddenRow {
    kind: String,
    text_key: String,
    text: String,
    hidden_at: String,
}

/// List remembered area names and items
///
/// Area names and miscellaneous / text items (description, last price,
/// quantity and Hide On Quote / Allow Discount / Taxable flags) typed on the
/// company's 500 most recently changed, not deleted quotes, to offer them
/// again while a quote is drawn. A name or item is offered only when quotes
/// of at least 2 different customers use it (a quote with no customer counts
/// by its deal, else on its own), so names typed for one job are not. Each
/// comes once (compared ignoring case and spaces), with the values of the
/// latest quote; used by most customers first, then most recent. Default area
/// names (`Area #1`, `Area #2`, ...) are left out. At most 60 names and 150
/// items. Suggestions taken out with `POST /v1/quotes/suggestions/hide` are
/// left out and listed in `hidden`, until quotes of 2 different customers
/// changed after that use them again.
#[crate::openapi::path(
    get,
    path = "/v1/quotes/suggestions",
    operation_id = "list_quote_suggestions",
    tag = "quotes",
    responses(
        (status = 200, description = "Remembered names and items", body = QuoteSuggestions),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn quote_suggestions(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<QuoteSuggestions>> {
    // Only the two lists are read out of each drawing, not the whole drawing.
    let rows: Vec<DrawingRow> = sqlx::query_as(
        "SELECT q.id, q.customer_id, q.deal_id,
                CAST(JSON_EXTRACT(q.draft_drawing_json, '$.areas') AS CHAR) AS areas,
                CAST(JSON_EXTRACT(q.draft_drawing_json, '$.items') AS CHAR) AS items,
                DATE_FORMAT(COALESCE(q.updated_at, q.created_date), '%Y-%m-%dT%H:%i:%sZ') AS used_at
           FROM quotes q
          WHERE q.company_id = ? AND q.deleted_at IS NULL AND q.draft_drawing_json IS NOT NULL
          ORDER BY COALESCE(q.updated_at, q.created_date) DESC, q.id DESC
          LIMIT ?",
    )
    .bind(user.company_id)
    .bind(RECENT_QUOTES)
    .fetch_all(&state.pool)
    .await?;
    let drawings: Vec<Drawing> = rows
        .into_iter()
        .map(|row| {
            let parse = |raw: Option<String>| {
                raw.and_then(|text| serde_json::from_str::<Value>(&text).ok())
                    .unwrap_or(Value::Null)
            };
            let owner = match (row.customer_id, row.deal_id) {
                (Some(customer), _) => format!("c{customer}"),
                (None, Some(deal)) => format!("d{deal}"),
                (None, None) => format!("q{}", row.id),
            };
            Drawing {
                areas: parse(row.areas),
                items: parse(row.items),
                used_at: row.used_at,
                owner,
            }
        })
        .collect();
    let hidden = load_hidden(&state.pool, user.company_id).await?;
    Ok(Json(rank_suggestions(&drawings, &hidden)))
}

/// The company's hidden suggestions by (kind, key). None while the
/// `quote_suggestion_hidden` migration has not run yet.
async fn load_hidden(
    pool: &sqlx::MySqlPool,
    company_id: i32,
) -> ApiResult<HashMap<(String, String), Hidden>> {
    let rows: Result<Vec<HiddenRow>, sqlx::Error> = sqlx::query_as(
        "SELECT kind, text_key, text,
                DATE_FORMAT(hidden_at, '%Y-%m-%dT%H:%i:%sZ') AS hidden_at
           FROM quote_suggestion_hidden
          WHERE company_id = ?",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(error) if is_missing_table(&error) => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                (row.kind, row.text_key),
                Hidden {
                    text: row.text,
                    hidden_at: row.hidden_at,
                },
            )
        })
        .collect())
}

/// MySQL's "table doesn't exist".
fn is_missing_table(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::code)
        .is_some_and(|code| code == "42S02")
}

/// Body of `POST /v1/quotes/suggestions/hide`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct HideSuggestionInput {
    /// `area` for an area name, `misc` or `text` for a custom item's description.
    pub kind: String,
    /// The suggestion as shown (any case; spaces around and inside are tidied).
    pub text: String,
}

/// Take a suggestion out of the list
///
/// The x next to an area name or a custom item's description in the quote
/// editor: `GET /v1/quotes/suggestions` stops offering it to everyone in the
/// company (it is listed in `hidden` instead). Quotes that use it are not
/// changed. It is offered again once quotes of 2 different customers, changed
/// after this, use it. Hiding the same text again moves that time to now.
/// Before the `quote_suggestion_hidden` table exists nothing is stored and it
/// still answers 204.
#[crate::openapi::path(
    post,
    path = "/v1/quotes/suggestions/hide",
    operation_id = "hide_quote_suggestion",
    tag = "quotes",
    request_body = HideSuggestionInput,
    responses(
        (status = 204, description = "Hidden"),
        (status = 400, description = "Unknown kind, or no text", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn hide_quote_suggestion(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<HideSuggestionInput>,
) -> ApiResult<StatusCode> {
    if !matches!(input.kind.as_str(), "area" | "misc" | "text") {
        return Err(ApiError::BadRequest(
            "kind must be area, misc or text".into(),
        ));
    }
    let Some(text) = clean(&input.text) else {
        return Err(ApiError::BadRequest(
            "Give the suggestion to hide (at most 200 characters).".into(),
        ));
    };
    let saved = sqlx::query(
        "INSERT INTO quote_suggestion_hidden (company_id, kind, text_key, text, hidden_by)
         VALUES (?, ?, ?, ?, ?)
         ON DUPLICATE KEY UPDATE text = VALUES(text), hidden_by = VALUES(hidden_by),
                                 hidden_at = CURRENT_TIMESTAMP",
    )
    .bind(user.company_id)
    .bind(&input.kind)
    .bind(text.to_lowercase())
    .bind(&text)
    .bind(user.id)
    .execute(&state.pool)
    .await;
    match saved {
        Ok(_) => Ok(StatusCode::NO_CONTENT),
        Err(error) if is_missing_table(&error) => {
            lambda_http::tracing::warn!(
                "quote_suggestion_hidden is missing: the x is not stored yet"
            );
            Ok(StatusCode::NO_CONTENT)
        }
        Err(error) => Err(error.into()),
    }
}

/// `Area #3`, `area #12`: the names the editor gives new areas.
fn is_default_area_name(name: &str) -> bool {
    let lower = name.trim().to_lowercase();
    let Some(rest) = lower.strip_prefix("area") else {
        return false;
    };
    let rest = rest.trim_start();
    let digits = rest.strip_prefix('#').unwrap_or(rest).trim();
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

/// Collapses runs of spaces and trims; None when empty or too long.
fn clean(text: &str) -> Option<String> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty() && text.chars().count() <= MAX_TEXT).then_some(text)
}

fn flag(item: &Value, key: &str, default: bool) -> bool {
    item.get(key).and_then(Value::as_bool).unwrap_or(default)
}

/// A misc or text line as a suggestion, with the values it has on this quote.
fn item_suggestion(
    item: &Value,
    kind: &str,
    description: String,
    used_at: Option<&String>,
) -> ItemSuggestion {
    ItemSuggestion {
        kind: kind.to_string(),
        description,
        price: if kind == "misc" {
            item.get("price").and_then(Value::as_f64)
        } else {
            None
        },
        quantity: item
            .get("quantity")
            .and_then(Value::as_f64)
            .filter(|q| q.is_finite() && *q > 0.0)
            .unwrap_or(1.0),
        hide_on_quote: flag(item, "hideOnQuote", false),
        allow_discount: flag(item, "allowDiscount", true),
        taxable: flag(item, "taxable", true),
        uses: 0,
        customers: 0,
        last_used: used_at.cloned(),
    }
}

/// Who uses one suggestion: the owners of every quote using it, and of the
/// quotes changed after it was hidden.
#[derive(Default)]
struct Owners {
    all: HashSet<String>,
    since_hidden: HashSet<String>,
}

impl Owners {
    fn add(&mut self, drawing: &Drawing, hidden: Option<&Hidden>) {
        self.all.insert(drawing.owner.clone());
        if let (Some(hidden), Some(used_at)) = (hidden, &drawing.used_at)
            && used_at.as_str() > hidden.hidden_at.as_str()
        {
            self.since_hidden.insert(drawing.owner.clone());
        }
    }

    /// Offered: enough customers, and when hidden, enough since then.
    fn offered(&self, hidden: bool) -> bool {
        self.all.len() >= MIN_CUSTOMERS && (!hidden || self.since_hidden.len() >= MIN_CUSTOMERS)
    }
}

/// Keeps the offered suggestions, used by most customers first (ties: seen
/// first, i.e. most recent), and notes the hidden ones quotes brought back.
fn offered<K: Eq + std::hash::Hash, S>(
    found: HashMap<K, (usize, S, Owners)>,
    hidden_key: impl Fn(&K) -> (String, String),
    hidden: &HashMap<(String, String), Hidden>,
    back: &mut HashSet<(String, String)>,
    set_customers: impl Fn(&mut S, i64),
) -> Vec<S> {
    let mut kept: Vec<(usize, i64, S)> = found
        .into_iter()
        .filter_map(|(key, (order, mut suggestion, owners))| {
            let key = hidden_key(&key);
            let was_hidden = hidden.contains_key(&key);
            if !owners.offered(was_hidden) {
                return None;
            }
            if was_hidden {
                back.insert(key);
            }
            let customers = i64::try_from(owners.all.len()).unwrap_or(i64::MAX);
            set_customers(&mut suggestion, customers);
            Some((order, customers, suggestion))
        })
        .collect();
    kept.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    kept.into_iter().map(|(_, _, s)| s).collect()
}

/// Counts names and items over drawings given newest first: one use per
/// quote, the spelling and values of the newest one. Keeps those of 2 or more
/// customers that are not hidden, used by most customers first.
fn rank_suggestions(
    drawings: &[Drawing],
    hidden: &HashMap<(String, String), Hidden>,
) -> QuoteSuggestions {
    // key -> (order first seen in, newest first; suggestion; who uses it)
    let mut order = 0usize;
    let mut names: HashMap<String, (usize, AreaNameSuggestion, Owners)> = HashMap::new();
    let mut items: HashMap<(String, String), (usize, ItemSuggestion, Owners)> = HashMap::new();
    for drawing in drawings {
        let used_at = &drawing.used_at;
        let mut seen_names: Vec<String> = Vec::new();
        for area in drawing.areas.as_array().into_iter().flatten() {
            let Some(name) = area.get("name").and_then(Value::as_str).and_then(clean) else {
                continue;
            };
            if is_default_area_name(&name) {
                continue;
            }
            let key = name.to_lowercase();
            if seen_names.contains(&key) {
                continue;
            }
            seen_names.push(key.clone());
            let hid = hidden.get(&("area".to_string(), key.clone()));
            let entry = names.entry(key).or_insert_with(|| {
                order += 1;
                (
                    order,
                    AreaNameSuggestion {
                        name,
                        uses: 0,
                        customers: 0,
                        last_used: used_at.clone(),
                    },
                    Owners::default(),
                )
            });
            entry.1.uses += 1;
            entry.2.add(drawing, hid);
        }
        let mut seen_items: Vec<(String, String)> = Vec::new();
        for item in drawing.items.as_array().into_iter().flatten() {
            let kind = item.get("kind").and_then(Value::as_str).unwrap_or_default();
            if kind != "misc" && kind != "text" {
                continue;
            }
            let Some(description) = item.get("name").and_then(Value::as_str).and_then(clean) else {
                continue;
            };
            let key = (kind.to_string(), description.to_lowercase());
            if seen_items.contains(&key) {
                continue;
            }
            seen_items.push(key.clone());
            let hid = hidden.get(&key);
            let entry = items.entry(key).or_insert_with(|| {
                order += 1;
                (
                    order,
                    item_suggestion(item, kind, description, used_at.as_ref()),
                    Owners::default(),
                )
            });
            entry.1.uses += 1;
            entry.2.add(drawing, hid);
        }
    }
    // Hidden ones that new quotes brought back are no longer hidden.
    let mut back: HashSet<(String, String)> = HashSet::new();
    let area_names = offered(
        names,
        |key| ("area".to_string(), key.clone()),
        hidden,
        &mut back,
        |s, customers| {
            s.customers = customers;
        },
    );
    let items = offered(items, Clone::clone, hidden, &mut back, |s, customers| {
        s.customers = customers;
    });
    let mut still_hidden: Vec<HiddenSuggestion> = hidden
        .iter()
        .filter(|(key, _)| !back.contains(*key))
        .map(|((kind, _), h)| HiddenSuggestion {
            kind: kind.clone(),
            text: h.text.clone(),
        })
        .collect();
    still_hidden.sort_by(|a, b| (&a.kind, &a.text).cmp(&(&b.kind, &b.text)));
    QuoteSuggestions {
        area_names: area_names.into_iter().take(MAX_AREA_NAMES).collect(),
        items: items.into_iter().take(MAX_ITEMS).collect(),
        hidden: still_hidden,
    }
}

/// Query of `GET /v1/quotes/stone-image`.
#[derive(Debug, Deserialize, IntoParams)]
pub struct StoneImageQuery {
    /// Color name as picked on the quote (any case), e.g. `Calacatta Laza`.
    #[serde(default)]
    pub color: String,
    /// Product (material) it was picked under (any case); a stone of this
    /// type is preferred, any type is used otherwise. May be empty.
    #[serde(default)]
    pub product: String,
}

/// `GET /v1/quotes/stone-image`.
#[derive(Debug, Serialize, ToSchema)]
pub struct StoneImageLookup {
    /// The color's photo; null when the company's catalog has no stone of
    /// that name with a photo (e.g. an Other Color typed by hand).
    pub image: Option<StoneImage>,
}

/// Get a color's stone photo
///
/// The catalog photo of one color as the quote shows it on the customer's
/// preview: the company's in-stock stone with this name (regular stock, or a
/// slab not sold, cut or deleted; a stone of the given product type first,
/// one with free slabs first). Returns `{"image": null}` when no stone of
/// that name is in stock or `color` is empty. The editor uses
/// it to paint the area's pieces in the Color & Edge preview.
#[crate::openapi::path(
    get,
    path = "/v1/quotes/stone-image",
    operation_id = "get_quote_stone_image",
    tag = "quotes",
    params(StoneImageQuery),
    responses(
        (status = 200, description = "The photo, or null", body = StoneImageLookup),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn quote_stone_image(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Query(query): Query<StoneImageQuery>,
) -> ApiResult<Json<StoneImageLookup>> {
    let color = query.color.trim();
    if color.is_empty() {
        return Ok(Json(StoneImageLookup { image: None }));
    }
    // The same lookup as the quote's previews, for a drawing with this one pick.
    let doc = json!({
        "areas": [{ "id": "a" }],
        "colorOptions": [{ "byArea": { "a": { "color": color, "product": query.product.trim() } } }],
    });
    let image = stone_images(&state.pool, user.company_id, &doc, true)
        .await?
        .into_iter()
        .next();
    Ok(Json(StoneImageLookup { image }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drawing(areas: Value, items: Value, used_at: &str, owner: &str) -> Drawing {
        Drawing {
            areas,
            items,
            used_at: Some(used_at.to_string()),
            owner: owner.to_string(),
        }
    }

    fn hide(kind: &str, text: &str, at: &str) -> ((String, String), Hidden) {
        (
            (kind.to_string(), text.to_lowercase()),
            Hidden {
                text: text.to_string(),
                hidden_at: at.to_string(),
            },
        )
    }

    #[test]
    fn default_area_names_are_skipped() {
        assert!(is_default_area_name("Area #1"));
        assert!(is_default_area_name(" area #12 "));
        assert!(is_default_area_name("Area 3"));
        assert!(!is_default_area_name("Area"));
        assert!(!is_default_area_name("Kitchen"));
        assert!(!is_default_area_name("Area #1 island"));
    }

    #[test]
    fn ranks_by_customers_then_recency_with_latest_values() {
        let drawings = vec![
            drawing(
                json!([{ "name": "kitchen " }, { "name": "Area #2" }, { "name": "Bar" }]),
                json!([
                    { "kind": "misc", "name": "Haul away", "price": 150.0, "taxable": false },
                    { "kind": "priceList", "name": "Sink cutout" },
                    { "kind": "text", "name": "Customer supplies sink", "price": 5 },
                ]),
                "2026-10-10T00:00:00Z",
                "c1",
            ),
            drawing(
                json!([
                    { "name": "Kitchen" },
                    { "name": "Island" },
                    { "name": "KITCHEN" },
                    { "name": "Bar" },
                ]),
                json!([
                    { "kind": "misc", "name": "haul away", "price": 100.0 },
                    { "kind": "text", "name": "Customer supplies sink" },
                ]),
                "2026-10-01T00:00:00Z",
                "c2",
            ),
            drawing(
                json!([{ "name": "Kitchen" }]),
                json!([]),
                "2026-09-01T00:00:00Z",
                "c3",
            ),
        ];
        let out = rank_suggestions(&drawings, &HashMap::new());
        let names: Vec<&str> = out.area_names.iter().map(|n| n.name.as_str()).collect();
        // Island is on one customer's quote only.
        assert_eq!(names, vec!["kitchen", "Bar"]);
        assert_eq!(out.area_names[0].uses, 3);
        assert_eq!(out.area_names[0].customers, 3);
        assert_eq!(out.items.len(), 2);
        let haul = &out.items[0];
        assert_eq!(haul.description, "Haul away");
        assert_eq!(haul.uses, 2);
        assert_eq!(haul.customers, 2);
        assert_eq!(haul.price, Some(150.0));
        assert!(!haul.taxable);
        assert!(haul.allow_discount);
        let text = &out.items[1];
        assert_eq!(text.kind, "text");
        assert_eq!(text.price, None);
        assert_eq!(out.hidden, vec![]);
    }

    #[test]
    fn quotes_of_one_customer_count_once() {
        // Copies and revisions of one customer's quote are one customer.
        let drawings = vec![
            drawing(
                json!([{ "name": "Unit 6211" }]),
                json!([]),
                "2026-10-03T00:00:00Z",
                "c7",
            ),
            drawing(
                json!([{ "name": "unit 6211" }]),
                json!([]),
                "2026-10-02T00:00:00Z",
                "c7",
            ),
            drawing(
                json!([{ "name": "Unit 6211" }]),
                json!([]),
                "2026-10-01T00:00:00Z",
                "c7",
            ),
        ];
        let out = rank_suggestions(&drawings, &HashMap::new());
        assert_eq!(out.area_names, vec![]);
    }

    #[test]
    fn hidden_ones_are_left_out_until_two_new_customers_use_them() {
        let at = "2026-10-05T00:00:00Z";
        let hidden: HashMap<(String, String), Hidden> = [
            hide("area", "Kitchen", at),
            hide("misc", "Trip fee", at),
            hide("area", "Vanity", at),
        ]
        .into_iter()
        .collect();
        let trip = json!([{ "kind": "misc", "name": "Trip fee", "price": 50 }]);
        let drawings = vec![
            // After hiding: Kitchen by c1 and c2, Trip fee by c1 only.
            drawing(
                json!([{ "name": "Kitchen" }]),
                trip.clone(),
                "2026-10-07T00:00:00Z",
                "c1",
            ),
            drawing(
                json!([{ "name": "Kitchen" }]),
                json!([]),
                "2026-10-06T00:00:00Z",
                "c2",
            ),
            // Before hiding.
            drawing(
                json!([{ "name": "Kitchen" }]),
                trip.clone(),
                "2026-10-01T00:00:00Z",
                "c3",
            ),
            drawing(json!([]), trip, "2026-09-01T00:00:00Z", "c4"),
        ];
        let out = rank_suggestions(&drawings, &hidden);
        let names: Vec<&str> = out.area_names.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["Kitchen"]);
        assert_eq!(out.items, vec![]);
        let still: Vec<(&str, &str)> = out
            .hidden
            .iter()
            .map(|h| (h.kind.as_str(), h.text.as_str()))
            .collect();
        assert_eq!(still, vec![("area", "Vanity"), ("misc", "Trip fee")]);
    }
}
