//! A company's sales tax, worked out from its address: nobody types it.
//!
//! The rate (state + county + city + district) comes from OpenTaxRate
//! (`api.opentaxrate.com`, built from the states' own published rate files,
//! no key). It is looked up when the address is saved and once a month by
//! `POST /v1/jobs/company-taxes`, and stored in `company.state_taxes` (a
//! fraction, 0.07 for 7%) with `state_taxes_checked_at`.

use crate::error::ApiError;
use crate::state::AppState;
use lambda_http::tracing;
use serde::Deserialize;
use sqlx::MySqlPool;
use std::sync::Arc;
use std::time::Duration;

/// The rate service's base URL (`SALES_TAX_API_URL`; a stub in tests).
#[derive(Clone)]
pub struct SalesTaxApi(pub Arc<str>);

impl SalesTaxApi {
    pub fn from_env() -> Self {
        let base = std::env::var("SALES_TAX_API_URL")
            .ok()
            .filter(|url| !url.trim().is_empty())
            .unwrap_or_else(|| "https://api.opentaxrate.com".into());
        Self(Arc::from(base.trim_end_matches('/')))
    }

    fn of(state: &AppState) -> Arc<Self> {
        state.extensions.get_or_insert_with(Self::from_env)
    }
}

/// What the lookup found.
#[derive(Debug, Clone, PartialEq)]
pub enum TaxLookup {
    /// The combined rate, as a fraction (0.0725 for 7.25%).
    Found(f64),
    /// The service read the address and could not place it.
    NotFound,
    /// The service did not answer (busy, down or rate-limited): try later.
    Unavailable,
}

/// The hint shown when an address can't be placed.
pub const NOT_FOUND_HINT: &str = "We couldn't find this address on the map, so the sales tax \
     can't be worked out. Write the full address with street, city, state and ZIP code.";

#[derive(Deserialize)]
struct RateAnswer {
    rate: Option<f64>,
}

/// Reads the service's answer: `status` and body.
pub fn read_answer(status: u16, body: &str) -> TaxLookup {
    match status {
        200..=299 => serde_json::from_str::<RateAnswer>(body)
            .ok()
            .and_then(|answer| answer.rate)
            .filter(|rate| rate.is_finite() && (0.0..1.0).contains(rate))
            .map_or(TaxLookup::Unavailable, TaxLookup::Found),
        429 | 500..=599 => TaxLookup::Unavailable,
        _ => TaxLookup::NotFound,
    }
}

/// Looks up the rate at `address`.
pub async fn lookup(state: &AppState, address: &str) -> TaxLookup {
    let api = SalesTaxApi::of(state);
    let Ok(url) =
        reqwest::Url::parse_with_params(&format!("{}/v1/rate", api.0), &[("address", address)])
    else {
        tracing::warn!(base = %api.0, "bad SALES_TAX_API_URL");
        return TaxLookup::Unavailable;
    };
    let sent = common::http::client()
        .get(url)
        .timeout(Duration::from_secs(20))
        .send()
        .await;
    let response = match sent {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(?error, "sales tax lookup failed");
            return TaxLookup::Unavailable;
        }
    };
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    let found = read_answer(status, &body);
    if found == TaxLookup::Unavailable {
        tracing::warn!(status, "sales tax lookup did not answer");
    }
    found
}

/// The stored form of a rate: a fraction with 4 decimals.
pub fn stored(rate: f64) -> String {
    format!("{:.4}", (rate * 10_000.0).round() / 10_000.0)
}

/// The lookup for a new `address`; none when it is blank.
pub async fn lookup_address(state: &AppState, address: Option<&str>) -> Option<TaxLookup> {
    let address = address.map(str::trim).filter(|a| !a.is_empty())?;
    Some(lookup(state, address).await)
}

/// The tax for a new address the user typed: refused when the service can't
/// place it, so a wrong address never leaves a wrong tax behind.
pub async fn checked_address(
    state: &AppState,
    address: Option<&str>,
) -> Result<Option<TaxLookup>, ApiError> {
    let found = lookup_address(state, address).await;
    if found == Some(TaxLookup::NotFound) {
        return Err(ApiError::BadRequest(NOT_FOUND_HINT.into()));
    }
    Ok(found)
}

/// Saves what [`lookup_address`] found for the company's new address: the
/// rate when found, no tax when the address was cleared. When the service
/// did not answer, the old rate stays and the job asks again on its next run.
pub async fn apply(
    pool: &MySqlPool,
    company_id: i32,
    found: Option<&TaxLookup>,
) -> Result<(), sqlx::Error> {
    match found {
        Some(found) => store(pool, company_id, found).await,
        None => {
            sqlx::query(
                "UPDATE company SET state_taxes = NULL, state_taxes_checked_at = NULL WHERE id = ?",
            )
            .bind(company_id)
            .execute(pool)
            .await?;
            Ok(())
        }
    }
}

async fn store(pool: &MySqlPool, company_id: i32, found: &TaxLookup) -> Result<(), sqlx::Error> {
    match found {
        TaxLookup::Found(rate) => {
            sqlx::query(
                "UPDATE company SET state_taxes = ?, state_taxes_checked_at = NOW() WHERE id = ?",
            )
            .bind(stored(*rate))
            .bind(company_id)
            .execute(pool)
            .await?;
        }
        // Can't be placed: keep the old rate, ask again next month.
        TaxLookup::NotFound => {
            sqlx::query("UPDATE company SET state_taxes_checked_at = NOW() WHERE id = ?")
                .bind(company_id)
                .execute(pool)
                .await?;
        }
        // No answer: keep the old rate, ask again on the next run.
        TaxLookup::Unavailable => {
            sqlx::query("UPDATE company SET state_taxes_checked_at = NULL WHERE id = ?")
                .bind(company_id)
                .execute(pool)
                .await?;
        }
    }
    Ok(())
}

/// What a run of the refresh job did.
#[derive(Debug, Default)]
pub struct RefreshRun {
    /// Companies looked up.
    pub checked: usize,
    /// Companies whose rate changed.
    pub changed: usize,
}

/// Refreshes the tax of the company checked longest ago (never, or over 30
/// days ago). One per run: the service limits lookups without a key, and the
/// job runs every few minutes.
pub async fn refresh_due(state: &AppState) -> Result<RefreshRun, sqlx::Error> {
    let due: Option<(i32, String, Option<String>)> = sqlx::query_as(
        "SELECT id, address, CAST(state_taxes AS CHAR) FROM company
          WHERE deleted_at IS NULL AND TRIM(COALESCE(address, '')) <> ''
            AND (state_taxes_checked_at IS NULL
                 OR state_taxes_checked_at < NOW() - INTERVAL 30 DAY)
          ORDER BY state_taxes_checked_at IS NOT NULL, state_taxes_checked_at, id
          LIMIT 1",
    )
    .fetch_optional(&state.pool)
    .await?;
    let Some((company_id, address, before)) = due else {
        return Ok(RefreshRun::default());
    };
    let found = lookup(state, &address).await;
    if found == TaxLookup::Unavailable {
        // Leave it due; the next run asks again.
        return Ok(RefreshRun::default());
    }
    store(&state.pool, company_id, &found).await?;
    let changed = matches!(found, TaxLookup::Found(rate)
        if before.as_deref() != Some(stored(rate).as_str()));
    if changed {
        tracing::info!(company_id, ?before, ?found, "company sales tax changed");
    }
    Ok(RefreshRun {
        checked: 1,
        changed: usize::from(changed),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_rate_and_the_refusals() {
        assert_eq!(
            read_answer(200, r#"{"state":"OH","rate":0.08,"stateRate":0.0575}"#),
            TaxLookup::Found(0.08)
        );
        assert_eq!(
            read_answer(422, r#"{"code":"ADDRESS_NEEDS_CONFIRMATION"}"#),
            TaxLookup::NotFound
        );
        assert_eq!(
            read_answer(400, r#"{"code":"BAD_REQUEST"}"#),
            TaxLookup::NotFound
        );
        assert_eq!(read_answer(429, ""), TaxLookup::Unavailable);
        assert_eq!(read_answer(503, ""), TaxLookup::Unavailable);
        assert_eq!(read_answer(200, "not json"), TaxLookup::Unavailable);
        assert_eq!(read_answer(200, r#"{"rate":7}"#), TaxLookup::Unavailable);
    }

    #[test]
    fn stores_four_decimals() {
        assert_eq!(stored(0.07), "0.0700");
        assert_eq!(stored(0.072_5), "0.0725");
        assert_eq!(stored(0.1), "0.1000");
    }
}
