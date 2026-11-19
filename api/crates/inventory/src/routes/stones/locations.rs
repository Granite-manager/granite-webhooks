//! Locations: the companies of one business (GMQ TOPS Montgomery, Birmingham,
//! Mobile) share `company.location_group`. When Admin › Stones › Settings
//! turns on "Show all locations' inventory", the company's staff can read
//! another location's stones with `?location=<company id>` on the stone
//! lists, the stone and its slabs. Look only: every write stays in the
//! caller's own company.

use crate::error::{ApiError, ApiResult};
use crate::openapi::{IntoParams, ToSchema};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, MySqlPool};

/// Query string of the stone reads that can show another location.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LocationQuery {
    /// Another location's company id (`GET /v1/stones/settings` lists them).
    /// Omitted or the caller's own company: their own stones.
    pub location: Option<i32>,
}

/// A location of the caller's business.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow, ToSchema)]
pub struct StoneLocation {
    /// Company id; pass it as `location`.
    pub id: i32,
    /// Company name, e.g. "GMQ TOPS Mobile".
    pub name: String,
}

/// The company whose stones the caller reads: their own, or `location` when
/// it is another location of their business and their company shows all
/// locations' inventory.
pub async fn inventory_company(
    pool: &MySqlPool,
    company_id: i32,
    location: Option<i32>,
) -> ApiResult<i32> {
    let Some(location) = location.filter(|&id| id != company_id) else {
        return Ok(company_id);
    };
    let shared: Option<i32> = sqlx::query_scalar(
        "SELECT other.id FROM company me
           JOIN company other ON other.location_group = me.location_group
          WHERE me.id = ? AND other.id = ? AND me.show_all_locations_inventory = 1
            AND other.deleted_at IS NULL",
    )
    .bind(company_id)
    .bind(location)
    .fetch_optional(pool)
    .await?;
    shared.ok_or_else(|| {
        ApiError::Denied(
            "Other locations' inventory is off. Ask your admin to turn on \
             \"Show all locations' inventory\" in Admin › Stones › Settings."
                .to_owned(),
        )
    })
}

/// Every location of the company's business, its own included, by name;
/// empty when the company has no location group.
pub async fn locations(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<Vec<StoneLocation>, sqlx::Error> {
    sqlx::query_as(
        "SELECT other.id, COALESCE(other.name, '') AS name FROM company me
           JOIN company other ON other.location_group = me.location_group
          WHERE me.id = ? AND other.deleted_at IS NULL
          ORDER BY other.name, other.id",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await
}
