//! The sales order a sold quote makes for the Inventory Manager.
//!
//! Marking a quote sold with an option (`mark_quote_sold`) adds one pending
//! order: per color of that option, the rooms using it, their square feet,
//! the slabs the quote asks for (the slab layout's, or the estimate from the
//! pieces, as the quote prices them) and that many free slabs of the
//! inventory stone of that color, suggested. Everyone whose position has the
//! Confirm sales orders skill (Inventory Manager by default) gets a
//! notification; they confirm or change the slabs on the
//! Sales orders page (`/v1/sales-orders`). The sales rep is not told.
//!
//! Undoing the sale cancels the order while it is pending ([`cancel`]).

use super::Quote;
use super::pricing::{DEFAULT_SLAB, estimated_slab_count, pieces_sq_in};
use crate::auth::skills::{CONFIRM_SALES_ORDERS, company_skills};
use crate::error::ApiResult;
use sqlx::{MySql, MySqlPool, QueryBuilder};

/// `notifications.notification_type` of a new sales order.
pub const SALES_ORDER_NOTIFICATION: &str = "sales_order";

/// One color of the sold option.
#[derive(Debug, Default, PartialEq)]
struct Picked {
    product: String,
    color: String,
    rooms: Vec<String>,
    sq_in: f64,
    /// Slabs laid out per room (0: none, estimate from the pieces).
    laid: Vec<(String, usize)>,
}

/// The colors of option `index`, in the order their rooms come.
fn colors(quote: &Quote, index: usize) -> Vec<Picked> {
    let doc = quote.doc();
    let Some(option) = doc.color_options.get(index) else {
        return Vec::new();
    };
    let mut found: Vec<Picked> = Vec::new();
    for area in &doc.areas {
        let Some(picked) = option.by_area.get(&area.id) else {
            continue;
        };
        let color = picked.color.trim();
        if color.is_empty() {
            continue;
        }
        let key = |c: &Picked| {
            c.color.eq_ignore_ascii_case(color)
                && c.product.eq_ignore_ascii_case(picked.product.trim())
        };
        let pos = found.iter().position(key).unwrap_or_else(|| {
            found.push(Picked {
                product: picked.product.trim().to_string(),
                color: color.to_string(),
                ..Picked::default()
            });
            found.len() - 1
        });
        let entry = &mut found[pos];
        if !area.name.trim().is_empty() {
            entry.rooms.push(area.name.trim().to_string());
        }
        entry.sq_in += pieces_sq_in(&doc, &area.id);
        let laid = doc
            .layouts
            .get(&area.id)
            .and_then(|l| l.get(&option.id))
            .map_or(0, |l| l.slabs.len());
        entry.laid.push((area.id.clone(), laid));
    }
    found
}

/// Slabs a color asks for: each room's laid-out slabs, or its estimate from
/// its pieces on slabs of `size`.
fn slabs_asked(quote: &Quote, color: &Picked, size: (f64, f64)) -> i32 {
    let doc = quote.doc();
    let count: f64 = color
        .laid
        .iter()
        .map(|(area, laid)| {
            if *laid > 0 {
                #[allow(clippy::cast_precision_loss)]
                let laid = *laid as f64;
                laid
            } else {
                estimated_slab_count(pieces_sq_in(&doc, area), size)
            }
        })
        .sum();
    #[allow(clippy::cast_possible_truncation)]
    let count = count as i32;
    count
}

/// Makes the pending sales order of a quote sold with option `index`.
///
/// Not when the quote has a pending or confirmed one. Tells the Inventory
/// Managers: returns the ids of their new bell notifications, for the caller
/// to push to their phones at once.
#[allow(clippy::too_many_lines)]
pub async fn create(
    pool: &MySqlPool,
    quote: &Quote,
    index: i32,
    deal_id: Option<i64>,
    user_id: Option<i32>,
    amount: Option<f64>,
) -> ApiResult<Vec<i64>> {
    let Ok(option) = usize::try_from(index) else {
        return Ok(Vec::new());
    };
    // Admin › Quotes › Automation "Sales order on a sale": off, none is made.
    let enabled: bool = sqlx::query_scalar(
        "SELECT COALESCE(auto_sales_order, 0) <> 0 FROM company WHERE id = ?",
    )
    .bind(quote.company_id)
    .fetch_optional(pool)
    .await?
    .unwrap_or(false);
    if !enabled {
        return Ok(Vec::new());
    }
    let existing: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM sales_orders
          WHERE quote_id = ? AND company_id = ? AND status IN ('pending', 'confirmed') LIMIT 1",
    )
    .bind(quote.id)
    .bind(quote.company_id)
    .fetch_optional(pool)
    .await?;
    let colors = colors(quote, option);
    if existing.is_some() || colors.is_empty() {
        return Ok(Vec::new());
    }
    // Everyone whose position has the Confirm sales orders skill (Inventory
    // Manager by default) is told.
    let positions: Vec<i32> = company_skills(pool, quote.company_id)
        .await
        .into_iter()
        .filter(|(_, skills, _)| skills.contains(&CONFIRM_SALES_ORDERS))
        .map(|(position, ..)| position)
        .collect();
    let mut tx = pool.begin().await?;
    let order_id = sqlx::query(
        "INSERT INTO sales_orders (company_id, quote_id, option_index, customer_id, deal_id,
                                   seller_id, price)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(quote.company_id)
    .bind(quote.id)
    .bind(index)
    .bind(quote.customer_id)
    .bind(deal_id)
    .bind(quote.salesperson_id.or(user_id))
    .bind(amount.unwrap_or(0.0))
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    for (position, color) in colors.iter().enumerate() {
        // The stone of that color, the one with the most free slabs first.
        let stone: Option<(i32, Option<f64>, Option<f64>)> = sqlx::query_as(
            "SELECT s.id, CAST(s.length AS DOUBLE), CAST(s.width AS DOUBLE)
               FROM stones s
              WHERE s.company_id = ? AND s.deleted_at IS NULL AND LOWER(TRIM(s.name)) = LOWER(?)
              ORDER BY (SELECT COUNT(*) FROM slab_inventory si
                         WHERE (si.stone_id = s.id OR si.stone_id IN (
                                SELECT l.source_stone_id FROM stone_slab_links l
                                 WHERE l.stone_id = s.id))
                           AND si.sale_id IS NULL AND si.cut_date IS NULL
                           AND si.deleted_at IS NULL) DESC, s.id
              LIMIT 1",
        )
        .bind(quote.company_id)
        .bind(&color.color)
        .fetch_optional(&mut *tx)
        .await?;
        let size = match stone {
            Some((_, Some(length), Some(width))) if length > 0.0 && width > 0.0 => (length, width),
            _ => DEFAULT_SLAB,
        };
        let asked = slabs_asked(quote, color, size);
        let rooms: String = color.rooms.join(", ").chars().take(500).collect();
        let item_id = sqlx::query(
            "INSERT INTO sales_order_items (order_id, position, color, product, stone_id, rooms,
                                            square_feet, slabs_asked)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(order_id)
        .bind(i32::try_from(position).unwrap_or(0))
        .bind(color.color.chars().take(255).collect::<String>())
        .bind(color.product.chars().take(255).collect::<String>())
        .bind(stone.map(|s| s.0))
        .bind(rooms)
        .bind((color.sq_in / 144.0 * 100.0).round() / 100.0)
        .bind(asked)
        .execute(&mut *tx)
        .await?
        .last_insert_id();
        let Some((stone_id, ..)) = stone else {
            continue;
        };
        // Free whole slabs no other pending order holds, oldest first.
        let slabs: Vec<i64> = sqlx::query_scalar(
            "SELECT CAST(si.id AS SIGNED) FROM slab_inventory si
              WHERE (si.stone_id = ? OR si.stone_id IN (
                     SELECT l.source_stone_id FROM stone_slab_links l WHERE l.stone_id = ?))
                AND si.sale_id IS NULL AND si.cut_date IS NULL AND si.deleted_at IS NULL
                AND si.parent_id IS NULL AND COALESCE(si.is_leftover, 0) = 0
                AND NOT EXISTS (
                    SELECT 1 FROM sales_order_slabs os
                      JOIN sales_order_items oi ON oi.id = os.item_id
                      JOIN sales_orders o ON o.id = oi.order_id
                     WHERE os.slab_id = si.id AND o.status = 'pending')
              ORDER BY si.id
              LIMIT ?",
        )
        .bind(stone_id)
        .bind(stone_id)
        .bind(i64::from(asked.max(0)))
        .fetch_all(&mut *tx)
        .await?;
        for slab in slabs {
            sqlx::query(
                "INSERT INTO sales_order_slabs (item_id, slab_id, is_full) VALUES (?, ?, 1)",
            )
            .bind(item_id)
            .bind(slab)
            .execute(&mut *tx)
            .await?;
        }
    }
    let customer: Option<Option<String>> =
        sqlx::query_scalar("SELECT name FROM customers WHERE id = ?")
            .bind(quote.customer_id)
            .fetch_optional(&mut *tx)
            .await?;
    let customer = customer.flatten().unwrap_or_default();
    let message: String = format!("New sales order to confirm: {}", customer.trim())
        .trim_end_matches([' ', ':'])
        .chars()
        .take(255)
        .collect();
    let managers: Vec<i32> = if positions.is_empty() {
        Vec::new()
    } else {
        let mut query = QueryBuilder::<MySql>::new(
            "SELECT DISTINCT u.id FROM users u
               JOIN users_positions up ON up.user_id = u.id
              WHERE u.is_deleted = 0 AND up.company_id = ",
        );
        query.push_bind(quote.company_id);
        query.push(" AND up.position_id IN (");
        let mut list = query.separated(", ");
        for position in &positions {
            list.push_bind(*position);
        }
        query.push(")");
        query.build_query_scalar().fetch_all(&mut *tx).await?
    };
    let mut notifications = Vec::with_capacity(managers.len());
    for manager in managers {
        let id = sqlx::query(
            "INSERT INTO notifications (user_id, customer_id, message, notification_type, due_at)
             VALUES (?, ?, ?, ?, NOW())",
        )
        .bind(manager)
        .bind(quote.customer_id)
        .bind(&message)
        .bind(SALES_ORDER_NOTIFICATION)
        .execute(&mut *tx)
        .await?
        .last_insert_id();
        notifications.push(i64::try_from(id).unwrap_or_default());
    }
    tx.commit().await?;
    Ok(notifications)
}

/// Undo sale: the quote's pending order is cancelled (a confirmed one stays:
/// its slabs are sold; the Inventory Manager unsells them on the stone).
pub async fn cancel(pool: &MySqlPool, quote_id: i32, company_id: i32) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE sales_orders SET status = 'cancelled'
          WHERE quote_id = ? AND company_id = ? AND status = 'pending'",
    )
    .bind(quote_id)
    .bind(company_id)
    .execute(pool)
    .await?;
    Ok(())
}
