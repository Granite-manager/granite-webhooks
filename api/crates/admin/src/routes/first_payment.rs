//! A company from the sign-up page pays its first subscription (ENG-90).
//!
//! - `GET  /v1/billing/first-payment`          → its monthly price, free time, who to contact
//! - `POST /v1/billing/first-payment/checkout` → a Stripe Checkout link to pay it
//!
//! The price is the company's own (`company.subscription_price_cents`,
//! $800 at sign-up; a superuser may change it on the Payments screen). A
//! company a superuser gave free time (`company.free_access_until`) saves
//! its card now and is first charged when the free time ends. Managers only.

use crate::auth::BillingManager;
use crate::billing::machine::has_running_subscription;
use crate::billing::store::{BillingStore, MySqlBillingStore};
use crate::billing::{CompanyBilling, is_ended_subscription_status};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::routes::billing_account::{Redirect, billing_url};
use crate::routes::payments::{PRODUCT_ID, PRODUCT_NAME, PRODUCT_TAX_CODE};
use crate::routes::signup::SALES_EMAIL;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::Serialize;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get_first_payment))
        .routes(routes!(first_payment_checkout))
}

/// Stripe's smallest USD charge.
const MIN_CHARGE_CENTS: i64 = 50;
/// Stripe takes a Checkout trial only this far ahead (it asks for 48 hours).
const TRIAL_END_MIN_HOURS: i64 = 49;

/// When a company with free time until `free_until` that subscribes `now`
/// is first charged (unix seconds); None: charged at checkout.
pub fn first_charge_at(free_until: Option<NaiveDateTime>, now: NaiveDateTime) -> Option<i64> {
    let ends = free_until?;
    (ends - now >= chrono::Duration::hours(TRIAL_END_MIN_HOURS)).then(|| ends.and_utc().timestamp())
}

#[derive(sqlx::FromRow)]
struct CompanyPrice {
    name: Option<String>,
    subscription_price_cents: i32,
    free_access_until: Option<NaiveDateTime>,
}

async fn company_price(state: &AppState, company_id: i32) -> ApiResult<CompanyPrice> {
    sqlx::query_as(
        "SELECT name, subscription_price_cents, free_access_until FROM company WHERE id = ?",
    )
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(ApiError::NotFound)
}

/// `GET /v1/billing/first-payment`.
#[derive(Debug, Serialize, ToSchema)]
pub struct FirstPayment {
    /// What the company pays a month, in USD cents (80000: $800).
    pub monthly_price_cents: i64,
    /// Who to write to for a free month and a demo instead of paying now.
    pub contact_email: String,
    /// Granite Manager let the company in free until then (ISO 8601); its
    /// first charge waits for it. Null: no free time.
    pub free_access_until: Option<DateTime<Utc>>,
    /// A subscription is running already.
    pub subscribed: bool,
}

async fn subscribed(state: &AppState, company_id: i32) -> ApiResult<bool> {
    let record = MySqlBillingStore(&state.pool)
        .by_company(company_id)
        .await?;
    Ok(has_running_subscription(record.as_ref()))
}

/// Get what the company pays
///
/// Its monthly price, the free time Granite Manager gave it (if any) and
/// who to write to for a free month and a demo. Pay with
/// `POST /v1/billing/first-payment/checkout`. Managers only.
#[crate::openapi::path(
    get,
    path = "/v1/billing/first-payment",
    operation_id = "get_billing_first_payment",
    tag = "billing",
    responses(
        (status = 200, description = "The price", body = FirstPayment),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a manager of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_first_payment(
    State(state): State<AppState>,
    BillingManager(user): BillingManager,
) -> ApiResult<Json<FirstPayment>> {
    let price = company_price(&state, user.company_id).await?;
    Ok(Json(FirstPayment {
        monthly_price_cents: i64::from(price.subscription_price_cents),
        contact_email: SALES_EMAIL.into(),
        free_access_until: price
            .free_access_until
            .filter(|until| *until > crate::billing::now())
            .map(|until| until.and_utc()),
        subscribed: subscribed(&state, user.company_id).await?,
    }))
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

/// Pay the monthly subscription
///
/// A Stripe Checkout link for a monthly subscription at the company's price
/// (`GET /v1/billing/first-payment`), charged now, or when the free time
/// Granite Manager gave ends. Stripe returns to
/// `/admin/billing?checkout=success` (or `cancel`); access opens once Stripe
/// confirms the payment. 400 when Stripe is not set up or refused, 409
/// while a subscription is running. Only one such page is open at a time
/// (asked again, the open page is answered again; after a change of price
/// it is closed and a new one made), so the company cannot pay twice from
/// two pages. Managers only.
#[crate::openapi::path(
    post,
    path = "/v1/billing/first-payment/checkout",
    operation_id = "create_billing_first_payment_checkout",
    tag = "billing",
    responses(
        (status = 200, description = "The Checkout link", body = Redirect),
        (status = 400, description = "Stripe is not set up, or refused", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a manager of the company", body = ErrorBody),
        (status = 409, description = "The company already has a subscription", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn first_payment_checkout(
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
    let price = company_price(&state, user.company_id).await?;
    let cents = i64::from(price.subscription_price_cents);
    if cents < MIN_CHARGE_CENTS {
        return Err(ApiError::BadRequest(
            "Your price is not set. Contact Granite Manager.".into(),
        ));
    }
    let name = price
        .name
        .map(|name| name.trim().to_string())
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
            cents,
            &format!("{page}?checkout=success"),
            &format!("{page}?checkout=cancel"),
            first_charge_at(price.free_access_until, crate::billing::now()),
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
    use super::first_charge_at;
    use chrono::NaiveDateTime;

    fn at(text: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    #[test]
    fn free_time_delays_the_first_charge() {
        let ends = at("2026-12-19 00:00:00");
        assert_eq!(
            first_charge_at(Some(ends), at("2026-11-20 00:00:00")),
            Some(ends.and_utc().timestamp())
        );
        assert_eq!(first_charge_at(Some(ends), at("2026-12-18 00:00:00")), None);
        assert_eq!(first_charge_at(None, at("2026-11-20 00:00:00")), None);
    }
}
