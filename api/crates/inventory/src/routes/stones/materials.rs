//! Materials: the stones of one name grouped together (Fantasy Brown), each
//! stone being one bundle with its bundle and block number. Admin › Stones ›
//! Settings picks whether the company sees stones directly (the old view,
//! default) or materials first.
//!
//! Stones are linked to the material of their name here: on switching to
//! materials, on listing materials, and on saving a stone.

use super::queries::{list_stones, stones_with_slab_counts};
use super::schemas::{ListStonesQuery, SlabCounts, Stone};
use crate::auth::{AdminUser, ShopOrEmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::{JsonBody, MultiQuery};
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, MySqlConnection};
use std::collections::HashMap;

/// What the Stones page shows first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum StoneView {
    /// One card per stone (the old view).
    Stones,
    /// One card per material; open it for its stones (bundles), then slabs.
    Materials,
}

impl StoneView {
    fn parse(value: &str) -> Self {
        if value == "materials" {
            Self::Materials
        } else {
            Self::Stones
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Stones => "stones",
            Self::Materials => "materials",
        }
    }
}

/// `GET` and `PUT /v1/stones/settings`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StoneSettings {
    /// `stones` (default) or `materials`.
    pub stone_view: StoneView,
}

/// A material with the slab counts of its stones.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct StoneMaterial {
    /// Material id.
    pub id: i32,
    /// Name, e.g. "Fantasy Brown".
    pub name: String,
    /// Stone type, lowercase: `granite`, `quartz`...
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// Photo: the material's own, else the first stone photo.
    pub url: Option<String>,
    /// Stones (bundles) of the material that match the filters.
    pub stones: i64,
    /// Any of its stones is shown in catalogs.
    pub is_display: bool,
    /// Any of its stones is regular stock.
    pub regular_stock: bool,
    /// Installed project photos across its stones.
    pub installed: i64,
    /// Slab counts summed over its stones.
    pub slabs: SlabCounts,
}

/// An installed project photo of one of the material's stones.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct MaterialImage {
    /// Photo id (`installed_stones`).
    pub id: i32,
    /// Photo URL.
    pub url: Option<String>,
    /// Stone the photo was added to.
    pub stone_id: i32,
    /// That stone's bundle number.
    pub bundle_number: Option<String>,
}

/// One material with its stones and installed photos.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MaterialDetail {
    /// Material id.
    pub id: i32,
    /// Name, e.g. "Fantasy Brown".
    pub name: String,
    /// Stone type.
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// Photo: the material's own, else the first stone photo.
    pub url: Option<String>,
    /// Its stones by bundle number; hidden ones only for admins.
    pub stones: Vec<Stone>,
    /// Installed project photos of all its stones.
    pub images: Vec<MaterialImage>,
}

#[derive(FromRow)]
struct MaterialRow {
    id: i32,
    name: String,
    #[sqlx(rename = "type")]
    kind: Option<String>,
    url: Option<String>,
}

/// Links stones without a material (just `stone_id`, or all of the company)
/// to the material of their name, creating missing materials. Names match
/// case-insensitively, trimmed; stones without a name stay unlinked.
pub async fn link_materials(
    conn: &mut MySqlConnection,
    company_id: i32,
    stone_id: Option<i32>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT IGNORE INTO stone_materials (company_id, name, type, url)
         SELECT ?, TRIM(name), MIN(type), NULL FROM stones
         WHERE company_id = ? AND deleted_at IS NULL AND material_id IS NULL
           AND TRIM(COALESCE(name, '')) <> '' AND (? IS NULL OR id = ?)
         GROUP BY TRIM(name)",
    )
    .bind(company_id)
    .bind(company_id)
    .bind(stone_id)
    .bind(stone_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE stones JOIN stone_materials m
           ON m.company_id = stones.company_id AND m.name = TRIM(stones.name)
         SET stones.material_id = m.id
         WHERE stones.company_id = ? AND stones.material_id IS NULL
           AND stones.deleted_at IS NULL AND (? IS NULL OR stones.id = ?)",
    )
    .bind(company_id)
    .bind(stone_id)
    .bind(stone_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn stone_view(conn: &mut MySqlConnection, company_id: i32) -> Result<StoneView, sqlx::Error> {
    let value: Option<String> = sqlx::query_scalar("SELECT stone_view FROM company WHERE id = ?")
        .bind(company_id)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(StoneView::parse(value.as_deref().unwrap_or("stones")))
}

/// Get the stone settings
///
/// What the company's Stones page shows first: `stones` (one card per stone,
/// the default) or `materials` (one card per material, then its stones).
#[crate::openapi::path(
    get,
    path = "/v1/stones/settings",
    operation_id = "get_stone_settings",
    tag = "stones",
    responses(
        (status = 200, description = "The settings", body = StoneSettings),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, admin or of the shop", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_settings(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
) -> ApiResult<Json<StoneSettings>> {
    let mut conn = state.pool.acquire().await?;
    Ok(Json(StoneSettings {
        stone_view: stone_view(&mut conn, user.company_id).await?,
    }))
}

/// Save the stone settings
///
/// Admin › Stones › Settings. Switching to `materials` links every stone to
/// the material of its name (created when missing); `stones` keeps the
/// links, so switching back and forth loses nothing.
#[crate::openapi::path(
    put,
    path = "/v1/stones/settings",
    operation_id = "save_stone_settings",
    tag = "stones",
    request_body = StoneSettings,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Unknown `stone_view`", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn save_settings(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<StoneSettings>,
) -> ApiResult<StatusCode> {
    let mut tx = state.pool.begin().await?;
    sqlx::query("UPDATE company SET stone_view = ? WHERE id = ?")
        .bind(input.stone_view.as_str())
        .bind(user.company_id)
        .execute(&mut *tx)
        .await?;
    if input.stone_view == StoneView::Materials {
        link_materials(&mut tx, user.company_id, None).await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// List the company's materials
///
/// Takes the filters of `GET /v1/stones` and returns, sorted by name, every
/// material that has a matching stone, with the slab counts of those stones
/// summed and how many installed project photos the material has. Links
/// stones without a material first.
#[crate::openapi::path(
    get,
    path = "/v1/stones/materials",
    operation_id = "list_stone_materials",
    tag = "stones",
    params(ListStonesQuery),
    responses(
        (status = 200, description = "Materials by name", body = Vec<StoneMaterial>),
        (status = 400, description = "A filter has the wrong type", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not staff, or asks for hidden stones without being an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    MultiQuery(query): MultiQuery<ListStonesQuery>,
) -> ApiResult<Json<Vec<StoneMaterial>>> {
    if query.include_hidden && !(user.is_admin || user.is_superuser) {
        return Err(ApiError::Forbidden);
    }
    let mut conn = state.pool.acquire().await?;
    link_materials(&mut conn, user.company_id, None).await?;
    let rows: Vec<MaterialRow> = sqlx::query_as(
        "SELECT id, name, type, url FROM stone_materials WHERE company_id = ? ORDER BY name, id",
    )
    .bind(user.company_id)
    .fetch_all(&mut *conn)
    .await?;
    let installed: HashMap<i32, i64> = sqlx::query_as::<_, (i32, i64)>(
        "SELECT stones.material_id, COUNT(*) FROM installed_stones i
         JOIN stones ON stones.id = i.stone_id
         WHERE stones.company_id = ? AND stones.deleted_at IS NULL
           AND stones.material_id IS NOT NULL
         GROUP BY stones.material_id",
    )
    .bind(user.company_id)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .collect();
    drop(conn);
    let stones = list_stones(&state.pool, user.company_id, &query).await?;
    Ok(Json(group(rows, &stones, &installed)))
}

fn group(
    rows: Vec<MaterialRow>,
    stones: &[Stone],
    installed: &HashMap<i32, i64>,
) -> Vec<StoneMaterial> {
    let mut by_material: HashMap<i32, Vec<&Stone>> = HashMap::new();
    for stone in stones {
        if let Some(id) = stone.material_id {
            by_material.entry(id).or_default().push(stone);
        }
    }
    rows.into_iter()
        .filter_map(|row| {
            let own = by_material.get(&row.id)?;
            let sum = |f: fn(&SlabCounts) -> i64| own.iter().map(|s| f(&s.slabs)).sum();
            Some(StoneMaterial {
                id: row.id,
                name: row.name,
                kind: row.kind.or_else(|| own.iter().find_map(|s| s.kind.clone())),
                url: row.url.or_else(|| own.iter().find_map(|s| s.url.clone())),
                stones: i64::try_from(own.len()).unwrap_or(i64::MAX),
                is_display: own.iter().any(|s| s.is_display),
                regular_stock: own.iter().any(|s| s.regular_stock),
                installed: installed.get(&row.id).copied().unwrap_or(0),
                slabs: SlabCounts {
                    total: sum(|c| c.total),
                    available: sum(|c| c.available),
                    whole_total: sum(|c| c.whole_total),
                    whole_available: sum(|c| c.whole_available),
                },
            })
        })
        .collect()
}

/// Get one material
///
/// The material with all its stones (bundles, sold out ones included;
/// hidden ones for admins only) sorted by bundle number, each with its slab
/// counts, and the installed project photos of every one of its stones.
#[crate::openapi::path(
    get,
    path = "/v1/stones/materials/{id}",
    operation_id = "get_stone_material",
    tag = "stones",
    params(("id" = i32, Path, description = "Material id")),
    responses(
        (status = 200, description = "The material", body = MaterialDetail),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, admin or of the shop", body = ErrorBody),
        (status = 404, description = "No such material in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<MaterialDetail>> {
    let row: MaterialRow = sqlx::query_as(
        "SELECT id, name, type, url FROM stone_materials WHERE id = ? AND company_id = ?",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(ApiError::NotFound)?;
    let mut query = stones_with_slab_counts(user.company_id);
    query.push(" AND stones.material_id = ").push_bind(id);
    if !(user.is_admin || user.is_superuser) {
        query.push(" AND stones.is_display = 1");
    }
    query.push(" GROUP BY stones.id ORDER BY stones.bundle_number IS NULL, stones.bundle_number, stones.id");
    let stones: Vec<Stone> = query.build_query_as().fetch_all(&state.pool).await?;
    let images: Vec<MaterialImage> = sqlx::query_as(
        "SELECT i.id, i.url, stones.id AS stone_id, stones.bundle_number
         FROM installed_stones i
         JOIN stones ON stones.id = i.stone_id
         WHERE stones.company_id = ? AND stones.material_id = ? AND stones.deleted_at IS NULL
         ORDER BY i.id DESC",
    )
    .bind(user.company_id)
    .bind(id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(MaterialDetail {
        id: row.id,
        name: row.name,
        url: row
            .url
            .or_else(|| stones.iter().find_map(|s| s.url.clone())),
        kind: row
            .kind
            .or_else(|| stones.iter().find_map(|s| s.kind.clone())),
        stones,
        images,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_view_reads_as_stones() {
        assert_eq!(StoneView::parse("materials"), StoneView::Materials);
        assert_eq!(StoneView::parse("stones"), StoneView::Stones);
        assert_eq!(StoneView::parse(""), StoneView::Stones);
    }
}
