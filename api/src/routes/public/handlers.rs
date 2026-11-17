use super::queries::{
    company_by_subdomain as find_company, find_material, find_stone, list_colors, list_fixtures,
    list_stones, material_photos, material_stones, search_stones as search, stone_photos,
    stone_slabs,
};
use super::schemas::{
    PublicCompaniesQuery, PublicCompany, PublicCompanyCard, PublicFixture, PublicFixturesQuery,
    PublicMaterial, PublicPhoto, PublicSearchQuery, PublicSlab, PublicStone, PublicStonesQuery,
};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::routes::catalog::schemas::Color;
use crate::routes::fixtures::{FAUCETS, SINKS};
use crate::routes::stones::materials::{StoneSettings, stone_view as company_stone_view};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};

/// List showroom companies
///
/// The name and logo of each live company among `ids`, by id, for the
/// customer portal's company chooser. No session needed.
#[crate::openapi::path(
    get,
    path = "/v1/public/companies",
    operation_id = "list_public_companies",
    tag = "public",
    params(PublicCompaniesQuery),
    responses(
        (status = 200, description = "The companies", body = Vec<PublicCompanyCard>),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn companies(
    State(state): State<AppState>,
    MultiQuery(query): MultiQuery<PublicCompaniesQuery>,
) -> ApiResult<Json<Vec<PublicCompanyCard>>> {
    let ids: Vec<i32> = query.ids.into_iter().take(20).collect();
    if ids.is_empty() {
        return Ok(Json(Vec::new()));
    }
    let marks = vec!["?"; ids.len()].join(",");
    let mut sql = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, name, NULLIF(NULLIF(TRIM(logo_url), ''), 'undefined') AS logo_url
           FROM company WHERE id IN ({marks}) AND deleted_at IS NULL ORDER BY id"
    )));
    for id in &ids {
        sql = sql.bind(id);
    }
    Ok(Json(sql.fetch_all(&state.pool).await?))
}

/// Find a company by its subdomain
///
/// Returns the id of the company whose showroom lives at
/// `<subdomain>.granite-manager.com`. No session needed.
#[crate::openapi::path(
    get,
    path = "/v1/public/companies/by-subdomain/{subdomain}",
    operation_id = "get_public_company_by_subdomain",
    tag = "public",
    params(("subdomain" = String, Path, description = "Subdomain, e.g. `granitedepotindy`")),
    responses(
        (status = 200, description = "The company", body = PublicCompany),
        (status = 404, description = "No live company has this subdomain", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn company_by_subdomain(
    State(state): State<AppState>,
    Path(subdomain): Path<String>,
) -> ApiResult<Json<PublicCompany>> {
    find_company(&state.pool, subdomain.trim())
        .await?
        .map(|id| Json(PublicCompany { id }))
        .ok_or(ApiError::NotFound)
}

/// List a company's showroom stones
///
/// Returns the company's displayed stones that are regular stock or have an
/// available whole slab, by name, with slab counts and retail prices (never
/// costs). Filters combine with AND; repeat list filters to match any value.
/// No session needed.
#[crate::openapi::path(
    get,
    path = "/v1/public/companies/{company_id}/stones",
    operation_id = "list_public_stones",
    tag = "public",
    params(("company_id" = i32, Path, description = "Company id"), PublicStonesQuery),
    responses(
        (status = 200, description = "Stones by name", body = Vec<PublicStone>),
        (status = 400, description = "A filter has the wrong type", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn stones(
    State(state): State<AppState>,
    Path(company_id): Path<i32>,
    MultiQuery(query): MultiQuery<PublicStonesQuery>,
) -> ApiResult<Json<Vec<PublicStone>>> {
    Ok(Json(list_stones(&state.pool, company_id, &query).await?))
}

/// Search a company's showroom stones by name
///
/// Same stones as the showroom list, filtered by name, best matches first
/// (name starts with it, then contains it as a word). No session needed.
#[crate::openapi::path(
    get,
    path = "/v1/public/companies/{company_id}/stones/search",
    operation_id = "search_public_stones",
    tag = "public",
    params(("company_id" = i32, Path, description = "Company id"), PublicSearchQuery),
    responses(
        (status = 200, description = "Matching stones, best first", body = Vec<PublicStone>),
        (status = 400, description = "A parameter has the wrong type", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn search_stones(
    State(state): State<AppState>,
    Path(company_id): Path<i32>,
    MultiQuery(query): MultiQuery<PublicSearchQuery>,
) -> ApiResult<Json<Vec<PublicStone>>> {
    Ok(Json(search(&state.pool, company_id, &query).await?))
}

/// Get one showroom stone
///
/// Returns a displayed stone of the company (sold out included). Hidden,
/// deleted and other companies' stones are 404. No session needed.
#[crate::openapi::path(
    get,
    path = "/v1/public/companies/{company_id}/stones/{stone_id}",
    operation_id = "get_public_stone",
    tag = "public",
    params(
        ("company_id" = i32, Path, description = "Company id"),
        ("stone_id" = i32, Path, description = "Stone id"),
    ),
    responses(
        (status = 200, description = "The stone", body = PublicStone),
        (status = 404, description = "Not a displayed stone of this company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn stone(
    State(state): State<AppState>,
    Path((company_id, stone_id)): Path<(i32, i32)>,
) -> ApiResult<Json<PublicStone>> {
    find_stone(&state.pool, company_id, stone_id)
        .await?
        .map(Json)
        .ok_or(ApiError::NotFound)
}

/// List a showroom stone's available slabs
///
/// Returns the slabs of a displayed stone (and of the stones it draws from)
/// that are in the yard and not sold, by id. No session needed.
#[crate::openapi::path(
    get,
    path = "/v1/public/companies/{company_id}/stones/{stone_id}/slabs",
    operation_id = "list_public_stone_slabs",
    tag = "public",
    params(
        ("company_id" = i32, Path, description = "Company id"),
        ("stone_id" = i32, Path, description = "Stone id"),
    ),
    responses(
        (status = 200, description = "Available slabs", body = Vec<PublicSlab>),
        (status = 404, description = "Not a displayed stone of this company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn stone_slabs_handler(
    State(state): State<AppState>,
    Path((company_id, stone_id)): Path<(i32, i32)>,
) -> ApiResult<Json<Vec<PublicSlab>>> {
    if find_stone(&state.pool, company_id, stone_id)
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound);
    }
    Ok(Json(stone_slabs(&state.pool, company_id, stone_id).await?))
}

/// List a showroom stone's installed photos
///
/// Returns project photos of a displayed stone, its own first, then those of
/// the stones it shows photos from. No session needed.
#[crate::openapi::path(
    get,
    path = "/v1/public/companies/{company_id}/stones/{stone_id}/images",
    operation_id = "list_public_stone_images",
    tag = "public",
    params(
        ("company_id" = i32, Path, description = "Company id"),
        ("stone_id" = i32, Path, description = "Stone id"),
    ),
    responses(
        (status = 200, description = "Photos", body = Vec<PublicPhoto>),
        (status = 404, description = "Not a displayed stone of this company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn stone_images(
    State(state): State<AppState>,
    Path((company_id, stone_id)): Path<(i32, i32)>,
) -> ApiResult<Json<Vec<PublicPhoto>>> {
    if find_stone(&state.pool, company_id, stone_id)
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound);
    }
    Ok(Json(stone_photos(&state.pool, company_id, stone_id).await?))
}

/// Get a company's stone view
///
/// What the company's Stones pages show first, set in Admin › Stones ›
/// Settings: `stones` (one card per stone, the default) or `materials` (one
/// card per material, then its stones). The showroom follows it. No session
/// needed.
#[crate::openapi::path(
    get,
    path = "/v1/public/companies/{company_id}/stone-view",
    operation_id = "get_public_stone_view",
    tag = "public",
    params(("company_id" = i32, Path, description = "Company id")),
    responses(
        (status = 200, description = "The stone view", body = StoneSettings),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn stone_view(
    State(state): State<AppState>,
    Path(company_id): Path<i32>,
) -> ApiResult<Json<StoneSettings>> {
    let mut conn = state.pool.acquire().await?;
    Ok(Json(StoneSettings {
        stone_view: company_stone_view(&mut conn, company_id).await?,
    }))
}

/// Get one showroom material
///
/// A material of the company (the stones of one name) with its displayed
/// stones in stock, by bundle number, and the installed photos of its
/// displayed stones. No prices: the stones' `retail_price` and
/// `slab_price_per_sqft` are null. A material without a stone in the
/// showroom is 404. No session needed.
#[crate::openapi::path(
    get,
    path = "/v1/public/companies/{company_id}/materials/{material_id}",
    operation_id = "get_public_material",
    tag = "public",
    params(
        ("company_id" = i32, Path, description = "Company id"),
        ("material_id" = i32, Path, description = "Material id"),
    ),
    responses(
        (status = 200, description = "The material", body = PublicMaterial),
        (status = 404, description = "No material of this company with a stone in the showroom", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn material(
    State(state): State<AppState>,
    Path((company_id, material_id)): Path<(i32, i32)>,
) -> ApiResult<Json<PublicMaterial>> {
    let (id, name, kind, url) = find_material(&state.pool, company_id, material_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let mut stones = material_stones(&state.pool, company_id, material_id).await?;
    if stones.is_empty() {
        return Err(ApiError::NotFound);
    }
    for stone in &mut stones {
        stone.retail_price = None;
        stone.slab_price_per_sqft = None;
    }
    let images = material_photos(&state.pool, company_id, material_id).await?;
    Ok(Json(PublicMaterial {
        id,
        name,
        kind: kind.or_else(|| stones.iter().find_map(|s| s.kind.clone())),
        url: url.or_else(|| stones.iter().find_map(|s| s.url.clone())),
        stones,
        images,
    }))
}

/// List a company's showroom sinks
///
/// Returns displayed sink models with a free unit or regular stock, by name.
/// No session needed.
#[crate::openapi::path(
    get,
    path = "/v1/public/companies/{company_id}/sinks",
    operation_id = "list_public_sinks",
    tag = "public",
    params(("company_id" = i32, Path, description = "Company id"), PublicFixturesQuery),
    responses(
        (status = 200, description = "Sink models by name", body = Vec<PublicFixture>),
        (status = 400, description = "A filter has the wrong type", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn sinks(
    State(state): State<AppState>,
    Path(company_id): Path<i32>,
    MultiQuery(query): MultiQuery<PublicFixturesQuery>,
) -> ApiResult<Json<Vec<PublicFixture>>> {
    Ok(Json(
        list_fixtures(&state.pool, SINKS, company_id, &query).await?,
    ))
}

/// List a company's showroom faucets
///
/// Returns displayed faucet models with a free unit or regular stock, by
/// name. No session needed.
#[crate::openapi::path(
    get,
    path = "/v1/public/companies/{company_id}/faucets",
    operation_id = "list_public_faucets",
    tag = "public",
    params(("company_id" = i32, Path, description = "Company id"), PublicFixturesQuery),
    responses(
        (status = 200, description = "Faucet models by name", body = Vec<PublicFixture>),
        (status = 400, description = "A filter has the wrong type", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn faucets(
    State(state): State<AppState>,
    Path(company_id): Path<i32>,
    MultiQuery(query): MultiQuery<PublicFixturesQuery>,
) -> ApiResult<Json<Vec<PublicFixture>>> {
    Ok(Json(
        list_fixtures(&state.pool, FAUCETS, company_id, &query).await?,
    ))
}

/// List the stone colors
///
/// Every color a stone can be tagged with, by name, for the showroom color
/// filter. No session needed.
#[crate::openapi::path(
    get,
    path = "/v1/public/colors",
    operation_id = "list_public_colors",
    tag = "public",
    responses(
        (status = 200, description = "Colors by name", body = Vec<Color>),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn colors(State(state): State<AppState>) -> ApiResult<Json<Vec<Color>>> {
    Ok(Json(list_colors(&state.pool).await?))
}
