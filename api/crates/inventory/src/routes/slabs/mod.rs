//! Slabs (`slab_inventory`): single slabs and remnants of a stone.
//!
//! - `GET /v1/slabs?id=..` → slabs by id with their stone
//! - `PATCH /v1/slabs/{id}` → change a slab's size and/or bundle label
//! - `DELETE /v1/slabs/{id}` → remove a slab from the yard
//! - `PUT /v1/slabs/{id}/hold` → hold an available slab for 3 to 15 days
//! - `DELETE /v1/slabs/{id}/hold` → take it off hold
//!
//! Listing slabs lives under the stone: `GET /v1/stones/{id}/slabs`.

mod handlers;
pub mod queries;
pub mod schemas;

use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::list))
        .routes(routes!(handlers::update, handlers::delete))
        .routes(routes!(handlers::hold, handlers::release))
}
