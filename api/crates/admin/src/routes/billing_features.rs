//! What a signed-up company pays: the features it picked (ENG-90).
//!
//! - `GET  /v1/billing/features`          → the priced features, the company's picks and its price
//! - `PUT  /v1/billing/features`          → change the picks (before the first payment)
//! - `POST /v1/billing/features/checkout` → a Stripe Checkout link to pay for them
//!
//! A company pays the sum of the monthly prices (`features.monthly_price_cents`)
//! of the features it picked, 20% off when it picks every priced feature.
//! Features without a price are not offered. The price is kept on the
//! company (`subscription_price_cents`, the Payments screen's price) and per
//! feature (`company_feature_choices`). Managers only.

use crate::auth::BillingManager;
use crate::billing::machine::has_running_subscription;
use crate::billing::store::{BillingStore, MySqlBillingStore};
use crate::billing::{CompanyBilling, is_ended_subscription_status};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::routes::billing_account::{Redirect, billing_url};
use crate::routes::payments::{PRODUCT_ID, PRODUCT_NAME, PRODUCT_TAX_CODE};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get_features, set_features))
        .routes(routes!(features_checkout))
}

/// Percent off when a company picks every priced feature.
pub const ALL_FEATURES_DISCOUNT_PERCENT: i64 = 20;
/// Stripe's smallest USD charge.
const MIN_CHARGE_CENTS: i64 = 50;

/// A feature with a monthly price.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow, ToSchema)]
pub struct PricedFeature {
    /// Feature id.
    pub id: i32,
    /// Its name, e.g. "Texting".
    pub name: String,
    /// What it costs a month, in USD cents.
    pub monthly_price_cents: i64,
}

/// What a set of picked features costs a month.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct FeatureQuote {
    /// The picked features that have a price, by name.
    pub features: Vec<PricedFeature>,
    /// Their prices added up, in USD cents.
    pub subtotal_cents: i64,
    /// Every priced feature is picked: `discount_percent` off.
    pub all_features: bool,
    /// Percent off for picking every priced feature (20).
    pub discount_percent: i64,
    /// Taken off the subtotal, in cents (0 unless `all_features`).
    pub discount_cents: i64,
    /// What the company pays a month, in USD cents.
    pub total_cents: i64,
}

/// What `chosen` (feature ids) costs out of `priced` (every priced feature).
/// Unknown and unpriced ids are left out.
pub fn quote(priced: &[PricedFeature], chosen: &[i32]) -> FeatureQuote {
    let features: Vec<PricedFeature> = priced
        .iter()
        .filter(|feature| chosen.contains(&feature.id))
        .cloned()
        .collect();
    let subtotal_cents: i64 = features.iter().map(|f| f.monthly_price_cents).sum();
    let all_features = !priced.is_empty() && features.len() == priced.len();
    let discount_cents = if all_features {
        (subtotal_cents * ALL_FEATURES_DISCOUNT_PERCENT + 50) / 100
    } else {
        0
    };
    FeatureQuote {
        features,
        subtotal_cents,
        all_features,
        discount_percent: ALL_FEATURES_DISCOUNT_PERCENT,
        discount_cents,
        total_cents: subtotal_cents - discount_cents,
    }
}

/// Every feature with a price, by name.
pub async fn priced_features(pool: &MySqlPool) -> sqlx::Result<Vec<PricedFeature>> {
    sqlx::query_as(
        "SELECT id, name, CAST(monthly_price_cents AS SIGNED) AS monthly_price_cents
           FROM features WHERE monthly_price_cents IS NOT NULL ORDER BY name, id",
    )
    .fetch_all(pool)
    .await
}

/// The features the company picked.
async fn chosen_ids(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Vec<i32>> {
    sqlx::query_scalar("SELECT feature_id FROM company_feature_choices WHERE company_id = ?")
        .bind(company_id)
        .fetch_all(pool)
        .await
}

/// Saves `quote` as the company's picks and its monthly price.
pub async fn save_choices(
    conn: &mut sqlx::MySqlConnection,
    company_id: i32,
    quote: &FeatureQuote,
) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM company_feature_choices WHERE company_id = ?")
        .bind(company_id)
        .execute(&mut *conn)
        .await?;
    for feature in &quote.features {
        sqlx::query(
            "INSERT INTO company_feature_choices (company_id, feature_id, monthly_price_cents)
             VALUES (?, ?, ?)",
        )
        .bind(company_id)
        .bind(feature.id)
        .bind(feature.monthly_price_cents)
        .execute(&mut *conn)
        .await?;
    }
    sqlx::query("UPDATE company SET subscription_price_cents = ? WHERE id = ?")
        .bind(quote.total_cents)
        .bind(company_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// `GET /v1/billing/features`.
#[derive(Debug, Serialize, ToSchema)]
pub struct BillingFeatures {
    /// Every feature that has a price, by name.
    pub available: Vec<PricedFeature>,
    /// Ids of the features the company picked.
    pub chosen_ids: Vec<i32>,
    /// What the picks cost a month.
    pub quote: FeatureQuote,
    /// The company has a running subscription: the picks can no longer be
    /// changed here (Granite Manager changes them).
    pub subscribed: bool,
}

async fn subscribed(state: &AppState, company_id: i32) -> ApiResult<bool> {
    let record = MySqlBillingStore(&state.pool)
        .by_company(company_id)
        .await?;
    Ok(has_running_subscription(record.as_ref()))
}

/// Get the features the company pays for
///
/// The priced features, the ones the company picked and what they cost a
/// month (20% off with every feature). Change the picks with
/// `PUT /v1/billing/features` and pay with
/// `POST /v1/billing/features/checkout`. Managers only.
#[crate::openapi::path(
    get,
    path = "/v1/billing/features",
    operation_id = "get_billing_features",
    tag = "billing",
    responses(
        (status = 200, description = "Features, picks and price", body = BillingFeatures),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a manager of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_features(
    State(state): State<AppState>,
    BillingManager(user): BillingManager,
) -> ApiResult<Json<BillingFeatures>> {
    let available = priced_features(&state.pool).await?;
    let chosen = chosen_ids(&state.pool, user.company_id).await?;
    Ok(Json(BillingFeatures {
        quote: quote(&available, &chosen),
        chosen_ids: chosen,
        available,
        subscribed: subscribed(&state, user.company_id).await?,
    }))
}

/// Body of `PUT /v1/billing/features`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct FeatureChoice {
    /// Ids of the priced features to pay for; at least one.
    pub feature_ids: Vec<i32>,
}

/// Change the features to pay for
///
/// Replaces the company's picks (priced features only, unknown ids left out)
/// and its monthly price; answers what they cost. Only before the first
/// payment: 409 once the company has a running subscription. Managers only.
#[crate::openapi::path(
    put,
    path = "/v1/billing/features",
    operation_id = "set_billing_features",
    tag = "billing",
    request_body = FeatureChoice,
    responses(
        (status = 200, description = "Saved; what they cost", body = FeatureQuote),
        (status = 400, description = "No priced feature picked", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a manager of the company", body = ErrorBody),
        (status = 409, description = "The company already has a subscription", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_features(
    State(state): State<AppState>,
    BillingManager(user): BillingManager,
    JsonBody(input): JsonBody<FeatureChoice>,
) -> ApiResult<Json<FeatureQuote>> {
    if subscribed(&state, user.company_id).await? {
        return Err(ApiError::Conflict(
            "Your subscription is running. Contact Granite Manager to change your features.".into(),
        ));
    }
    let available = priced_features(&state.pool).await?;
    let picked = quote(&available, &input.feature_ids);
    if picked.features.is_empty() {
        return Err(ApiError::BadRequest("Pick at least one feature".into()));
    }
    let mut conn = state.pool.acquire().await?;
    save_choices(&mut conn, user.company_id, &picked).await?;
    Ok(Json(picked))
}

/// The company's Stripe customer: the one on record, else a new one, saved
/// both for the billing gate (`company_billing`) and the Payments screen
/// (`company.stripe_customer_id`).
async fn customer(state: &AppState, company_id: i32, name: &str) -> ApiResult<String> {
    let store = MySqlBillingStore(&state.pool);
    let record = store.by_company(company_id).await?;
    let on_company: Option<String> =
        sqlx::query_scalar("SELECT stripe_customer_id FROM company WHERE id = ?")
            .bind(company_id)
            .fetch_optional(&state.pool)
            .await?
            .flatten()
            .filter(|id: &String| !id.trim().is_empty());
    let known = record
        .as_ref()
        .and_then(|r| r.stripe_customer_id.clone())
        .or_else(|| on_company.clone());
    let customer = if let Some(customer) = known {
        customer
    } else {
        let created = state
            .stripe
            .create_customer(name, company_id)
            .await
            .map_err(|err| ApiError::BadRequest(err.to_string()))?;
        created["id"]
            .as_str()
            .ok_or_else(|| ApiError::Upstream("Stripe returned no customer id".into()))?
            .to_string()
    };
    if record.as_ref().and_then(|r| r.stripe_customer_id.as_ref()) != Some(&customer) {
        store
            .save(&CompanyBilling {
                stripe_customer_id: Some(customer.clone()),
                ..record.unwrap_or_else(|| CompanyBilling::empty(company_id))
            })
            .await?;
    }
    if on_company.is_none() {
        sqlx::query(
            "UPDATE company SET stripe_customer_id = ?
              WHERE id = ? AND (stripe_customer_id IS NULL OR stripe_customer_id = '')",
        )
        .bind(&customer)
        .bind(company_id)
        .execute(&state.pool)
        .await?;
    }
    Ok(customer)
}

/// Pay for the picked features
///
/// A Stripe Checkout link for a monthly subscription at the price of the
/// company's picked features (`GET /v1/billing/features`), charged now.
/// Stripe returns to `/admin/billing?checkout=success` (or `cancel`); access
/// opens once Stripe confirms the payment. 400 when nothing priced is picked
/// or Stripe is not set up, 409 while a subscription is running. Managers
/// only.
#[crate::openapi::path(
    post,
    path = "/v1/billing/features/checkout",
    operation_id = "create_billing_features_checkout",
    tag = "billing",
    responses(
        (status = 200, description = "The Checkout link", body = Redirect),
        (status = 400, description = "Nothing to pay for, or Stripe refused", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a manager of the company", body = ErrorBody),
        (status = 409, description = "The company already has a subscription", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn features_checkout(
    State(state): State<AppState>,
    BillingManager(user): BillingManager,
    headers: HeaderMap,
) -> ApiResult<Json<Redirect>> {
    if !state.stripe.is_enabled() {
        return Err(ApiError::BadRequest("Billing is not configured".into()));
    }
    if subscribed(&state, user.company_id).await? {
        return Err(ApiError::Conflict("You already have a subscription".into()));
    }
    let available = priced_features(&state.pool).await?;
    let chosen = chosen_ids(&state.pool, user.company_id).await?;
    let picked = quote(&available, &chosen);
    if picked.features.is_empty() || picked.total_cents < MIN_CHARGE_CENTS {
        return Err(ApiError::BadRequest(
            "Pick the features to pay for first".into(),
        ));
    }
    // Prices may have changed since the picks were saved.
    let mut conn = state.pool.acquire().await?;
    save_choices(&mut conn, user.company_id, &picked).await?;
    drop(conn);
    let name: String = sqlx::query_scalar("SELECT name FROM company WHERE id = ?")
        .bind(user.company_id)
        .fetch_optional(&state.pool)
        .await?
        .flatten()
        .map(|name: String| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Company".into());
    let customer = customer(&state, user.company_id, &name).await?;
    // A subscription Stripe has that the webhook has not told us about yet.
    let live = state
        .stripe
        .list_subscriptions(&customer)
        .await
        .map_err(|err| ApiError::BadRequest(err.to_string()))?;
    if live.iter().any(|s| {
        s["status"]
            .as_str()
            .is_some_and(|status| status != "incomplete" && !is_ended_subscription_status(status))
    }) {
        return Err(ApiError::Conflict("You already have a subscription".into()));
    }
    state
        .stripe
        .ensure_product(PRODUCT_ID, PRODUCT_NAME, PRODUCT_TAX_CODE)
        .await
        .map_err(|err| ApiError::BadRequest(err.to_string()))?;
    let page = billing_url(&state, &headers);
    let session = state
        .stripe
        .create_monthly_checkout_session(
            &customer,
            user.company_id,
            PRODUCT_ID,
            picked.total_cents,
            &format!("{page}?checkout=success"),
            &format!("{page}?checkout=cancel"),
            None,
        )
        .await
        .map_err(|err| ApiError::BadRequest(err.to_string()))?;
    let url = session["url"]
        .as_str()
        .ok_or_else(|| ApiError::Upstream("Stripe returned no Checkout link".into()))?;
    Ok(Json(Redirect { url: url.into() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feature(id: i32, cents: i64) -> PricedFeature {
        PricedFeature {
            id,
            name: format!("F{id}"),
            monthly_price_cents: cents,
        }
    }

    #[test]
    fn picked_prices_add_up() {
        let priced = [feature(1, 10_000), feature(2, 5_000), feature(3, 2_500)];
        let picked = quote(&priced, &[1, 3, 99]);
        assert_eq!(picked.features, vec![feature(1, 10_000), feature(3, 2_500)]);
        assert_eq!(picked.subtotal_cents, 12_500);
        assert!(!picked.all_features);
        assert_eq!(picked.discount_cents, 0);
        assert_eq!(picked.total_cents, 12_500);
    }

    #[test]
    fn every_feature_is_twenty_percent_off() {
        let priced = [feature(1, 10_000), feature(2, 5_000), feature(3, 2_555)];
        let picked = quote(&priced, &[3, 2, 1]);
        assert!(picked.all_features);
        assert_eq!(picked.subtotal_cents, 17_555);
        assert_eq!(picked.discount_cents, 3_511);
        assert_eq!(picked.total_cents, 14_044);
    }

    #[test]
    fn nothing_priced_costs_nothing() {
        let picked = quote(&[], &[1, 2]);
        assert_eq!(picked.features, Vec::<PricedFeature>::new());
        assert!(!picked.all_features);
        assert_eq!(picked.total_cents, 0);
    }
}
