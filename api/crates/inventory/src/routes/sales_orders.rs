//! Sales orders: what a sold quote takes from the inventory, for the
//! Inventory Manager to confirm.
//!
//! - `GET  /v1/sales-orders`              → the company's orders (to confirm first)
//! - `GET  /v1/sales-orders/{id}`         → one order, its colors, slabs and the free slabs
//! - `PUT  /v1/sales-orders/{id}`         → save the slabs of a pending order
//! - `POST /v1/sales-orders/{id}/confirm` → save them and sell them
//!
//! Marking a quote sold makes the order (`api_quotes::quote_pricing::sales_order`)
//! with the slabs the quote asks for, suggested. The Inventory Manager keeps
//! them, picks others, uses only part of one (`is_full: false`: the rest
//! stays in the yard as a remnant) or adds and removes slabs. Confirming
//! sells them as Sell slab does (`POST /v1/sales`), one room per color.
//! Only the Sales orders tab opens this API (`SALES_ORDERS_TAB`).

use super::sales::queries::{sell, validate};
use super::sales::schemas::{RoomInput, SaleInput, SaleSlab};
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::{IntoParams, OpenApiRouter, ToSchema, routes};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{MySqlConnection, MySqlPool};
use std::collections::HashSet;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(get, save))
        .routes(routes!(confirm))
}

/// Query of `GET /v1/sales-orders`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// `pending` (the default: still to confirm), `confirmed` or `all`
    /// (cancelled ones are never listed).
    #[serde(default)]
    pub status: Option<String>,
}

/// One order in the list.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct SalesOrderRow {
    pub id: i32,
    /// `pending` or `confirmed`.
    pub status: String,
    /// When the quote was sold (UTC, ISO 8601).
    pub created_at: String,
    pub customer_id: i32,
    pub customer_name: Option<String>,
    /// Who the sale is credited to.
    pub seller_name: Option<String>,
    pub quote_id: i32,
    pub quote_name: Option<String>,
    /// Its colors, comma-separated.
    pub colors: Option<String>,
    /// Slabs the quote asks for.
    pub slabs_asked: i64,
    /// Slabs set on it (a part counts a half).
    pub slabs_set: f64,
}

/// A slab of the inventory.
#[derive(Debug, Clone, Serialize, ToSchema, sqlx::FromRow)]
pub struct SlabChoice {
    /// `slab_inventory.id`.
    pub id: i64,
    pub bundle: Option<String>,
    pub length: Option<f64>,
    pub width: Option<f64>,
    /// Its picture.
    pub url: Option<String>,
    /// A remnant (a piece left from another slab).
    pub remnant: bool,
}

/// A slab set on an order.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct OrderSlab {
    /// The slab.
    #[serde(flatten)]
    pub slab: SlabChoice,
    /// `false`: only part of it is used.
    pub is_full: bool,
}

/// One color of an order.
#[derive(Debug, Serialize, ToSchema)]
pub struct SalesOrderItem {
    pub id: i32,
    pub color: String,
    pub product: String,
    /// The rooms using it, comma-separated.
    pub rooms: String,
    pub square_feet: f64,
    /// Slabs the quote asks for.
    pub slabs_asked: i32,
    /// The inventory stone of the color; null when it is not in the inventory.
    pub stone_id: Option<i32>,
    pub stone_name: Option<String>,
    pub stone_url: Option<String>,
    /// The slabs set.
    pub slabs: Vec<OrderSlab>,
    /// Free slabs of the stone another pending order does not hold (those
    /// set here too), for a pending order.
    pub available: Vec<SlabChoice>,
}

/// One order.
#[derive(Debug, Serialize, ToSchema)]
pub struct SalesOrder {
    #[serde(flatten)]
    pub row: SalesOrderRow,
    /// The sold option, 0-based.
    pub option_index: i32,
    /// The option's total, in dollars.
    pub price: f64,
    /// The sale confirming made.
    pub sale_id: Option<i32>,
    pub confirmed_by_name: Option<String>,
    pub confirmed_at: Option<String>,
    pub items: Vec<SalesOrderItem>,
}

/// The slabs of one color.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ItemSlabsInput {
    /// `SalesOrderItem.id`.
    pub id: i32,
    pub slabs: Vec<SlabInput>,
}

/// A slab to set.
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SlabInput {
    pub slab_id: i64,
    /// `false`: only part of it.
    pub is_full: bool,
}

/// The slabs of an order, every color given.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SalesOrderInput {
    pub items: Vec<ItemSlabsInput>,
}

const ROW_SELECT: &str = "SELECT o.id, o.status,
        DATE_FORMAT(o.created_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at,
        o.customer_id, c.name AS customer_name, u.name AS seller_name,
        o.quote_id, q.quote_name,
        (SELECT GROUP_CONCAT(i.color ORDER BY i.position SEPARATOR ', ')
           FROM sales_order_items i WHERE i.order_id = o.id) AS colors,
        CAST(COALESCE((SELECT SUM(i.slabs_asked) FROM sales_order_items i
                        WHERE i.order_id = o.id), 0) AS SIGNED) AS slabs_asked,
        CAST(COALESCE((SELECT SUM(IF(s.is_full, 1, 0.5)) FROM sales_order_slabs s
                         JOIN sales_order_items i ON i.id = s.item_id
                        WHERE i.order_id = o.id), 0) AS DOUBLE) AS slabs_set
   FROM sales_orders o
   LEFT JOIN customers c ON c.id = o.customer_id
   LEFT JOIN users u ON u.id = o.seller_id
   LEFT JOIN quotes q ON q.id = o.quote_id";

/// List sales orders
///
/// The company's orders, newest first: the pending ones by default.
#[crate::openapi::path(
    get,
    path = "/v1/sales-orders",
    operation_id = "list_sales_orders",
    tag = "sales",
    params(ListQuery),
    responses(
        (status = 200, description = "The orders", body = Vec<SalesOrderRow>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin, or has no Sales orders tab", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Vec<SalesOrderRow>>> {
    let statuses = match query.status.as_deref() {
        None | Some("pending") => "'pending'",
        Some("confirmed") => "'confirmed'",
        Some("all") => "'pending', 'confirmed'",
        Some(_) => {
            return Err(ApiError::BadRequest(
                "status must be pending, confirmed or all".into(),
            ));
        }
    };
    let rows = sqlx::query_as::<_, SalesOrderRow>(sqlx::AssertSqlSafe(format!(
        "{ROW_SELECT} WHERE o.company_id = ? AND o.status IN ({statuses})
          ORDER BY o.status = 'pending' DESC, o.created_at DESC, o.id DESC LIMIT 500"
    )))
    .bind(user.company_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(rows))
}

#[derive(sqlx::FromRow)]
struct OrderFacts {
    option_index: i32,
    price: f64,
    sale_id: Option<i32>,
    confirmed_by_name: Option<String>,
    confirmed_at: Option<String>,
    deal_id: Option<i64>,
    seller_id: Option<i32>,
}

#[derive(sqlx::FromRow)]
struct ItemRow {
    id: i32,
    color: String,
    product: String,
    rooms: String,
    square_feet: f64,
    slabs_asked: i32,
    stone_id: Option<i32>,
    stone_name: Option<String>,
    stone_url: Option<String>,
}

const SLAB_SELECT: &str = "SELECT CAST(si.id AS SIGNED) AS id, si.bundle,
        CAST(si.length AS DOUBLE) AS length, CAST(si.width AS DOUBLE) AS width,
        NULLIF(si.url, 'undefined') AS url,
        (si.parent_id IS NOT NULL OR COALESCE(si.is_leftover, 0) <> 0) AS remnant
   FROM slab_inventory si";

/// Free slabs of `stone_id` (and the stones it takes slabs from) that no
/// pending order but `order_id` holds.
async fn available(
    conn: &mut MySqlConnection,
    stone_id: i32,
    order_id: i32,
) -> sqlx::Result<Vec<SlabChoice>> {
    sqlx::query_as::<_, SlabChoice>(sqlx::AssertSqlSafe(format!(
        "{SLAB_SELECT}
          WHERE (si.stone_id = ? OR si.stone_id IN (
                 SELECT l.source_stone_id FROM stone_slab_links l WHERE l.stone_id = ?))
            AND si.sale_id IS NULL AND si.cut_date IS NULL AND si.deleted_at IS NULL
            AND NOT EXISTS (
                SELECT 1 FROM sales_order_slabs os
                  JOIN sales_order_items oi ON oi.id = os.item_id
                  JOIN sales_orders o ON o.id = oi.order_id
                 WHERE os.slab_id = si.id AND o.status = 'pending' AND o.id <> ?)
          ORDER BY remnant, si.id"
    )))
    .bind(stone_id)
    .bind(stone_id)
    .bind(order_id)
    .fetch_all(conn)
    .await
}

async fn find(
    conn: &mut MySqlConnection,
    company_id: i32,
    id: i32,
) -> ApiResult<Option<SalesOrder>> {
    let row = sqlx::query_as::<_, SalesOrderRow>(sqlx::AssertSqlSafe(format!(
        "{ROW_SELECT} WHERE o.id = ? AND o.company_id = ? AND o.status <> 'cancelled'"
    )))
    .bind(id)
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let facts = order_facts(&mut *conn, id).await?;
    let items: Vec<ItemRow> = sqlx::query_as(
        "SELECT i.id, i.color, i.product, i.rooms, CAST(i.square_feet AS DOUBLE) AS square_feet,
                i.slabs_asked, i.stone_id, s.name AS stone_name,
                NULLIF(s.url, 'undefined') AS stone_url
           FROM sales_order_items i
           LEFT JOIN stones s ON s.id = i.stone_id
          WHERE i.order_id = ?
          ORDER BY i.position, i.id",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    let pending = row.status == "pending";
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let set: Vec<(i64, bool)> = sqlx::query_as(
            "SELECT CAST(slab_id AS SIGNED), is_full <> 0 FROM sales_order_slabs
              WHERE item_id = ? ORDER BY id",
        )
        .bind(item.id)
        .fetch_all(&mut *conn)
        .await?;
        let mut slabs = Vec::with_capacity(set.len());
        for (slab_id, is_full) in set {
            let slab: Option<SlabChoice> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "{SLAB_SELECT} WHERE si.id = ?"
            )))
            .bind(slab_id)
            .fetch_optional(&mut *conn)
            .await?;
            if let Some(slab) = slab {
                slabs.push(OrderSlab { slab, is_full });
            }
        }
        let available = match item.stone_id {
            Some(stone) if pending => available(&mut *conn, stone, id).await?,
            _ => Vec::new(),
        };
        out.push(SalesOrderItem {
            id: item.id,
            color: item.color,
            product: item.product,
            rooms: item.rooms,
            square_feet: item.square_feet,
            slabs_asked: item.slabs_asked,
            stone_id: item.stone_id,
            stone_name: item.stone_name,
            stone_url: item.stone_url,
            slabs,
            available,
        });
    }
    Ok(Some(SalesOrder {
        row,
        option_index: facts.option_index,
        price: facts.price,
        sale_id: facts.sale_id,
        confirmed_by_name: facts.confirmed_by_name,
        confirmed_at: facts.confirmed_at,
        items: out,
    }))
}

async fn order_facts(conn: &mut MySqlConnection, id: i32) -> sqlx::Result<OrderFacts> {
    sqlx::query_as(
        "SELECT o.option_index, CAST(o.price AS DOUBLE) AS price, o.sale_id,
                u.name AS confirmed_by_name,
                DATE_FORMAT(o.confirmed_at, '%Y-%m-%dT%H:%i:%sZ') AS confirmed_at,
                o.deal_id, o.seller_id
           FROM sales_orders o LEFT JOIN users u ON u.id = o.confirmed_by
          WHERE o.id = ?",
    )
    .bind(id)
    .fetch_one(conn)
    .await
}

/// Get a sales order
///
/// The order with each color's rooms, square feet, the slabs the quote asks
/// for, the slabs set and, while pending, the free slabs of its stone.
#[crate::openapi::path(
    get,
    path = "/v1/sales-orders/{id}",
    operation_id = "get_sales_order",
    tag = "sales",
    params(("id" = i32, Path, description = "Sales order id")),
    responses(
        (status = 200, description = "The order", body = SalesOrder),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin, or has no Sales orders tab", body = ErrorBody),
        (status = 404, description = "No such order in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<SalesOrder>> {
    let mut conn = state.pool.acquire().await?;
    find(&mut conn, user.company_id, id)
        .await?
        .map(Json)
        .ok_or(ApiError::NotFound)
}

/// Replaces the slabs of pending order `id` with `input` (every color of
/// the order given, each slab one of its stone's free ones).
async fn save_slabs(
    conn: &mut MySqlConnection,
    company_id: i32,
    id: i32,
    input: &SalesOrderInput,
) -> ApiResult<()> {
    let status: Option<String> = sqlx::query_scalar(
        "SELECT status FROM sales_orders WHERE id = ? AND company_id = ? FOR UPDATE",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?;
    match status.as_deref() {
        None | Some("cancelled") => return Err(ApiError::NotFound),
        Some("pending") => {}
        Some(_) => {
            return Err(ApiError::BadRequest(
                "This sales order is already confirmed.".into(),
            ));
        }
    }
    let items: Vec<(i32, Option<i32>, String)> =
        sqlx::query_as("SELECT id, stone_id, color FROM sales_order_items WHERE order_id = ?")
            .bind(id)
            .fetch_all(&mut *conn)
            .await?;
    let mut seen = HashSet::new();
    for given in &input.items {
        let Some((_, stone, color)) = items.iter().find(|(item, ..)| *item == given.id) else {
            return Err(ApiError::BadRequest(format!(
                "item {} is not on this sales order",
                given.id
            )));
        };
        let free: Vec<i64> = match stone {
            Some(stone) => available(&mut *conn, *stone, id)
                .await?
                .into_iter()
                .map(|slab| slab.id)
                .collect(),
            None => Vec::new(),
        };
        for slab in &given.slabs {
            if !seen.insert(slab.slab_id) {
                return Err(ApiError::BadRequest(format!(
                    "Slab {} is set twice. Pick another slab.",
                    slab.slab_id
                )));
            }
            if !free.contains(&slab.slab_id) {
                return Err(ApiError::BadRequest(format!(
                    "Slab {} is not a free slab of {color}: it was sold or is held by another sales order. Pick another slab.",
                    slab.slab_id
                )));
            }
        }
    }
    for given in &input.items {
        sqlx::query("DELETE FROM sales_order_slabs WHERE item_id = ?")
            .bind(given.id)
            .execute(&mut *conn)
            .await?;
        for slab in &given.slabs {
            sqlx::query(
                "INSERT INTO sales_order_slabs (item_id, slab_id, is_full) VALUES (?, ?, ?)",
            )
            .bind(given.id)
            .bind(slab.slab_id)
            .bind(slab.is_full)
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

/// Save a sales order's slabs
///
/// Sets the slabs of the colors given (others keep theirs): each a free
/// slab of the color's stone no other pending order holds, `is_full: false`
/// for part of one. Only a pending order changes. Returns the order.
#[crate::openapi::path(
    put,
    path = "/v1/sales-orders/{id}",
    operation_id = "save_sales_order",
    tag = "sales",
    params(("id" = i32, Path, description = "Sales order id")),
    request_body = SalesOrderInput,
    responses(
        (status = 200, description = "Saved", body = SalesOrder),
        (status = 400, description = "Already confirmed, an unknown color, or a slab sold, held or set twice", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin, or has no Sales orders tab", body = ErrorBody),
        (status = 404, description = "No such order in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn save(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<SalesOrderInput>,
) -> ApiResult<Json<SalesOrder>> {
    let mut tx = state.pool.begin().await?;
    save_slabs(&mut tx, user.company_id, id, &input).await?;
    tx.commit().await?;
    order(&state.pool, user.company_id, id).await
}

async fn order(pool: &MySqlPool, company_id: i32, id: i32) -> ApiResult<Json<SalesOrder>> {
    let mut conn = pool.acquire().await?;
    find(&mut conn, company_id, id)
        .await?
        .map(Json)
        .ok_or(ApiError::NotFound)
}

/// Confirm a sales order
///
/// Saves the slabs given (as `PUT /v1/sales-orders/{id}`), then sells them
/// to the order's customer as Sell slab does: one room per color, credited
/// to the quote's sales rep, priced at the option's total; a slab used in
/// part leaves a remnant. Colors without slabs are left out; with none at
/// all, nothing is sold. All or nothing. Returns the order.
#[crate::openapi::path(
    post,
    path = "/v1/sales-orders/{id}/confirm",
    operation_id = "confirm_sales_order",
    tag = "sales",
    params(("id" = i32, Path, description = "Sales order id")),
    request_body = SalesOrderInput,
    responses(
        (status = 200, description = "Confirmed", body = SalesOrder),
        (status = 400, description = "Already confirmed, an unknown color, a slab sold, held or set twice, or the customer was deleted", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin, or has no Sales orders tab", body = ErrorBody),
        (status = 404, description = "No such order in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn confirm(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<SalesOrderInput>,
) -> ApiResult<Json<SalesOrder>> {
    let mut tx = state.pool.begin().await?;
    save_slabs(&mut tx, user.company_id, id, &input).await?;
    let facts = order_facts(&mut tx, id).await?;
    let customer_id: i32 = sqlx::query_scalar("SELECT customer_id FROM sales_orders WHERE id = ?")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    let items: Vec<(i32, String, String, f64)> = sqlx::query_as(
        "SELECT id, color, rooms, CAST(square_feet AS DOUBLE) FROM sales_order_items
          WHERE order_id = ? ORDER BY position, id",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    let mut rooms = Vec::new();
    for (item, color, names, square_feet) in items {
        let slabs: Vec<(i64, bool)> = sqlx::query_as(
            "SELECT CAST(slab_id AS SIGNED), is_full <> 0 FROM sales_order_slabs
              WHERE item_id = ? ORDER BY id",
        )
        .bind(item)
        .fetch_all(&mut *tx)
        .await?;
        if slabs.is_empty() {
            continue;
        }
        let room_id: String = sqlx::query_scalar("SELECT UUID()")
            .fetch_one(&mut *tx)
            .await?;
        let name = if names.trim().is_empty() {
            color
        } else {
            names
        };
        rooms.push(RoomInput {
            room_id,
            room: name.chars().take(100).collect(),
            backsplash: "no".into(),
            // A sale needs square feet; a quote without pieces has none.
            square_feet: Some(if square_feet > 0.0 { square_feet } else { 1.0 }),
            retail_price: 0.0,
            tear_out: "no".into(),
            stove: "f/s".into(),
            waterfall: "no".into(),
            corbels: 0,
            seam: "standard".into(),
            ten_year_sealer: false,
            extras: json!({}),
            slabs: slabs
                .into_iter()
                .map(|(id, is_full)| SaleSlab { id, is_full })
                .collect(),
            sinks: Vec::new(),
            faucets: Vec::new(),
        });
    }
    let sale_id = if rooms.is_empty() {
        None
    } else {
        let sale = SaleInput {
            customer_id,
            deal_id: Some(facts.deal_id.unwrap_or(0)),
            seller_id: facts.seller_id,
            project_address: None,
            notes: None,
            price: facts.price,
            extras: json!([]),
            rooms,
        };
        validate(&mut tx, user.company_id, &sale, None).await?;
        let name = user.name.clone().unwrap_or_default();
        Some(sell(&mut tx, user.company_id, (user.id, &name), &sale).await?)
    };
    sqlx::query(
        "UPDATE sales_orders SET status = 'confirmed', sale_id = ?, confirmed_by = ?,
                confirmed_at = CURRENT_TIMESTAMP
          WHERE id = ?",
    )
    .bind(sale_id)
    .bind(user.id)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    order(&state.pool, user.company_id, id).await
}
