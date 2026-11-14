//! `GET /v1/reports/cut-slabs`: whole slabs the shop cut, and
//! `GET /v1/reports/slabs`: slabs by status (available, on hold, sold), for the admin
//! Reports page.
//!
//! Replaces the Remix `admin.reports` loader.

use crate::auth::AdminUser;
use crate::error::{ApiResult, ErrorBody};
use crate::openapi::{IntoParams, ToSchema};
use crate::openapi::{OpenApiRouter, routes};
use crate::serde_helpers::js_date;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{MySql, QueryBuilder};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(cut_slabs))
        .routes(routes!(slabs_by_status))
}

/// Query of `GET /v1/reports/cut-slabs`. Every filter is optional.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CutSlabsQuery {
    /// Cut on or after this day, `YYYY-MM-DD`.
    pub from: Option<NaiveDate>,
    /// Cut on or before this day, `YYYY-MM-DD`.
    pub to: Option<NaiveDate>,
    /// Only stones of this supplier.
    pub supplier_id: Option<i32>,
    /// Only this stone.
    pub stone_id: Option<i32>,
}

/// A cut whole slab with its stone, supplier and sale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow, ToSchema)]
pub struct CutSlab {
    /// Slab id.
    pub id: i64,
    /// Bundle label.
    pub bundle: Option<String>,
    /// Stone id.
    pub stone_id: i32,
    /// Stone name.
    pub stone_name: Option<String>,
    /// Supplier id.
    pub supplier_id: i32,
    /// Supplier name.
    pub supplier_name: Option<String>,
    /// When it was cut.
    #[serde(serialize_with = "js_date")]
    pub cut_date: Option<DateTime<Utc>>,
    /// When it was sold; null when the sale is missing.
    #[serde(serialize_with = "js_date")]
    pub sale_date: Option<DateTime<Utc>>,
    /// Who sold it; null when unknown.
    pub seller_name: Option<String>,
}

/// Cut slabs report
///
/// Whole slabs (not remnants) of the caller's company that were cut, with
/// their stone, the stone's supplier and the sale, newest cut first. Slabs
/// of stones without a supplier are left out.
#[crate::openapi::path(
    get,
    path = "/v1/reports/cut-slabs",
    operation_id = "get_cut_slabs_report",
    tag = "reports",
    params(CutSlabsQuery),
    responses(
        (status = 200, description = "Cut slabs", body = Vec<CutSlab>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn cut_slabs(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Query(query): Query<CutSlabsQuery>,
) -> ApiResult<Json<Vec<CutSlab>>> {
    let mut sql = QueryBuilder::<MySql>::new(
        "SELECT CAST(si.id AS SIGNED) AS id, si.bundle, stones.id AS stone_id,
                stones.name AS stone_name, suppliers.id AS supplier_id,
                suppliers.supplier_name, si.cut_date, sales.sale_date,
                users.name AS seller_name
           FROM slab_inventory si
           JOIN stones ON si.stone_id = stones.id
           JOIN suppliers ON stones.supplier_id = suppliers.id
           LEFT JOIN sales ON si.sale_id = sales.id
           LEFT JOIN users ON sales.seller_id = users.id
          WHERE si.cut_date IS NOT NULL AND si.parent_id IS NULL
            AND stones.company_id = ",
    );
    sql.push_bind(user.company_id)
        .push(" AND suppliers.company_id = ")
        .push_bind(user.company_id);
    if let Some(from) = query.from {
        sql.push(" AND DATE(si.cut_date) >= ").push_bind(from);
    }
    if let Some(to) = query.to {
        sql.push(" AND DATE(si.cut_date) <= ").push_bind(to);
    }
    if let Some(supplier_id) = query.supplier_id {
        sql.push(" AND suppliers.id = ").push_bind(supplier_id);
    }
    if let Some(stone_id) = query.stone_id {
        sql.push(" AND stones.id = ").push_bind(stone_id);
    }
    sql.push(" ORDER BY si.cut_date DESC, si.id DESC");
    Ok(Json(sql.build_query_as().fetch_all(&state.pool).await?))
}

/// Which slabs `GET /v1/reports/slabs` lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SlabStatus {
    /// In the yard, unsold and not on hold.
    Available,
    /// Unsold and on hold (the hold has not ended yet).
    OnHold,
    /// Part of a sale.
    Sold,
}

/// Query of `GET /v1/reports/slabs`. Every filter but `status` is optional.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SlabsByStatusQuery {
    /// Which slabs: `available`, `on_hold` or `sold`.
    pub status: SlabStatus,
    /// On or after this day, `YYYY-MM-DD`: the day the slab was added
    /// (available), put on hold (on hold) or sold (sold).
    pub from: Option<NaiveDate>,
    /// On or before this day, `YYYY-MM-DD`; same day as `from`.
    pub to: Option<NaiveDate>,
    /// Only stones of this supplier.
    pub supplier_id: Option<i32>,
    /// Only this stone.
    pub stone_id: Option<i32>,
}

/// A slab or remnant with its stone, supplier, hold and sale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow, ToSchema)]
pub struct StatusSlab {
    /// Slab id.
    pub id: i64,
    /// Bundle label.
    pub bundle: Option<String>,
    /// A remnant cut from another slab (false for a whole slab).
    pub is_remnant: bool,
    /// Length in inches as entered.
    pub length: Option<String>,
    /// Width in inches as entered.
    pub width: Option<String>,
    /// Stone id.
    pub stone_id: i32,
    /// Stone name.
    pub stone_name: Option<String>,
    /// Supplier id; null when the stone has none.
    pub supplier_id: Option<i32>,
    /// Supplier name.
    pub supplier_name: Option<String>,
    /// When the slab was added.
    #[serde(serialize_with = "js_date")]
    pub added_date: Option<DateTime<Utc>>,
    /// When the hold ends (on hold only).
    #[serde(serialize_with = "js_date")]
    pub hold_until: Option<DateTime<Utc>>,
    /// When it was put on hold (on hold only).
    #[serde(serialize_with = "js_date")]
    pub held_at: Option<DateTime<Utc>>,
    /// Who put it on hold (on hold only).
    pub held_by_name: Option<String>,
    /// When it was sold (sold only).
    #[serde(serialize_with = "js_date")]
    pub sale_date: Option<DateTime<Utc>>,
    /// Who sold it (sold only).
    pub seller_name: Option<String>,
    /// Who bought it (sold only).
    pub customer_name: Option<String>,
}

/// Slabs by status report
///
/// Slabs and remnants of the caller's company (deleted ones left out) with
/// the given status: `available` (in the yard, unsold, not on hold),
/// `on_hold` (unsold, hold not ended yet) or `sold` (part of a sale, cut or
/// not). `from`/`to` filter on the day the slab was added, put on hold or
/// sold. Newest first.
#[crate::openapi::path(
    get,
    path = "/v1/reports/slabs",
    operation_id = "get_slabs_by_status_report",
    tag = "reports",
    params(SlabsByStatusQuery),
    responses(
        (status = 200, description = "Slabs", body = Vec<StatusSlab>),
        (status = 400, description = "Missing or unknown status", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn slabs_by_status(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Query(query): Query<SlabsByStatusQuery>,
) -> ApiResult<Json<Vec<StatusSlab>>> {
    let on_hold = "si.sale_id IS NULL AND si.hold_until > UTC_TIMESTAMP()";
    let mut sql = QueryBuilder::<MySql>::new(format!(
        "SELECT CAST(si.id AS SIGNED) AS id, si.bundle,
                (si.parent_id IS NOT NULL) AS is_remnant, si.length, si.width,
                stones.id AS stone_id, stones.name AS stone_name,
                suppliers.id AS supplier_id, suppliers.supplier_name,
                CAST(si.created_at AS DATETIME) AS added_date,
                CASE WHEN {on_hold} THEN si.hold_until END AS hold_until,
                CASE WHEN {on_hold} THEN si.hold_at END AS held_at,
                CASE WHEN {on_hold} THEN holder.name END AS held_by_name,
                sales.sale_date, seller.name AS seller_name,
                customers.name AS customer_name
           FROM slab_inventory si
           JOIN stones ON si.stone_id = stones.id
           LEFT JOIN suppliers ON stones.supplier_id = suppliers.id
           LEFT JOIN users holder ON holder.id = si.hold_by
           LEFT JOIN sales ON si.sale_id = sales.id
           LEFT JOIN users seller ON sales.seller_id = seller.id
           LEFT JOIN customers ON sales.customer_id = customers.id
          WHERE si.deleted_at IS NULL AND stones.company_id = "
    ));
    sql.push_bind(user.company_id);
    let day = match query.status {
        SlabStatus::Available => {
            sql.push(
                " AND si.sale_id IS NULL AND si.cut_date IS NULL
                  AND (si.hold_until IS NULL OR si.hold_until <= UTC_TIMESTAMP())",
            );
            "si.created_at"
        }
        SlabStatus::OnHold => {
            sql.push(format!(" AND si.cut_date IS NULL AND {on_hold}"));
            "si.hold_at"
        }
        SlabStatus::Sold => {
            sql.push(" AND si.sale_id IS NOT NULL");
            "sales.sale_date"
        }
    };
    if let Some(from) = query.from {
        sql.push(format!(" AND DATE({day}) >= ")).push_bind(from);
    }
    if let Some(to) = query.to {
        sql.push(format!(" AND DATE({day}) <= ")).push_bind(to);
    }
    if let Some(supplier_id) = query.supplier_id {
        sql.push(" AND suppliers.id = ").push_bind(supplier_id);
    }
    if let Some(stone_id) = query.stone_id {
        sql.push(" AND stones.id = ").push_bind(stone_id);
    }
    sql.push(format!(" ORDER BY {day} DESC, si.id DESC"));
    Ok(Json(sql.build_query_as().fetch_all(&state.pool).await?))
}
