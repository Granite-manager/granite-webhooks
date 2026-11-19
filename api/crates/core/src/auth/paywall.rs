//! A company from the sign-up page has no access until it has paid.
//!
//! `company.source = 'signup_page'` (ENG-90): every authenticated request in
//! such a company is refused with 403, whichever credential it came with
//! (session cookie, agent token, the phone's device token), until Stripe
//! shows its subscription paid, or while a superuser gave it free time
//! (`company.free_access_until`, a free month with a demo). Open meanwhile: sign-in and sign-out
//! (`/v1/auth/...`), billing (`/v1/billing...`, where it pays) and the app
//! shell the billing page is drawn in. Superusers are never stopped.
//!
//! "Paid" is read from `company_billing`, which only Stripe writes: its
//! signed webhooks (`/v1/webhooks/stripe`, each event re-read from Stripe's
//! API) and syncs that ask Stripe's API. Nothing the browser says counts.
//! A database error refuses the request (the caller gets a 500), never lets
//! it through; `company.billing_required` plays no part.
//!
//! A company found paid is remembered for `PAID_CACHE_SECS` per `AppState`
//! (one per Lambda instance), so the check costs one primary-key query a minute per company;
//! an unpaid one is never remembered, so paying opens access at once.

use super::session::SessionUser;
use crate::error::ApiError;
use crate::state::AppState;
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// `company.source` of a company that signed itself up.
pub const SIGNUP_PAGE: &str = "signup_page";
/// How long a company found paid (or not from the sign-up page) is believed.
const PAID_CACHE_SECS: u64 = 60;
/// What a refused request is told.
pub const PAYMENT_REQUIRED: &str = "Your Granite Manager subscription is not paid yet. Pay on the Billing page to open your account.";

/// Companies found open, and when: per `AppState` (one per Lambda instance).
#[derive(Default)]
struct OpenCompanies(Mutex<HashMap<i32, Instant>>);

/// The routes a company that has not paid may still call.
pub fn open_while_unpaid(path: &str) -> bool {
    path.starts_with("/v1/auth/")
        || path == "/v1/billing"
        || path.starts_with("/v1/billing/")
        || path == "/v1/app/shell"
}

/// Whether a company is kept out, in SQL.
///
/// It came from the sign-up page, has no free time a superuser gave it
/// (`free_access_until` in the future) and no paid Stripe subscription
/// (active, a Stripe trial, or a failed renewal still in its grace period).
pub const BLOCKED_SQL: &str = "SELECT c.source = 'signup_page'
       AND NOT COALESCE(c.free_access_until > UTC_TIMESTAMP(), FALSE)
       AND NOT EXISTS (
        SELECT 1 FROM company_billing cb
         WHERE cb.company_id = c.id
           AND cb.stripe_subscription_id IS NOT NULL
           AND cb.subscription_status IN ('active', 'trialing', 'past_due')
           AND (cb.payment_status = 'active'
                OR (cb.payment_status = 'past_due'
                    AND cb.grace_period_ends_at > UTC_TIMESTAMP())))
      FROM company c WHERE c.id = ?";

/// Whether `company_id` must pay before it gets in, asked now.
pub async fn company_blocked(pool: &MySqlPool, company_id: i32) -> sqlx::Result<bool> {
    let blocked: Option<bool> = sqlx::query_scalar(BLOCKED_SQL)
        .bind(company_id)
        .fetch_optional(pool)
        .await?;
    Ok(blocked.unwrap_or(false))
}

impl OpenCompanies {
    fn remembered(&self, company_id: i32) -> bool {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&company_id)
            .is_some_and(|at| at.elapsed() < Duration::from_secs(PAID_CACHE_SECS))
    }

    fn remember(&self, company_id: i32) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(company_id, Instant::now());
    }
}

/// Refuses `user`'s request to `path` while their acting company has to pay
/// first.
pub async fn check(state: &AppState, user: &SessionUser, path: &str) -> Result<(), ApiError> {
    if user.is_superuser || open_while_unpaid(path) {
        return Ok(());
    }
    let open = state.extensions.get_or_insert_with(OpenCompanies::default);
    if open.remembered(user.company_id) {
        return Ok(());
    }
    if company_blocked(&state.pool, user.company_id).await? {
        return Err(ApiError::Denied(PAYMENT_REQUIRED.into()));
    }
    open.remember(user.company_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::open_while_unpaid;

    #[test]
    fn only_sign_in_and_billing_stay_open() {
        for open in [
            "/v1/auth/session",
            "/v1/auth/logout",
            "/v1/billing",
            "/v1/billing/gate",
            "/v1/billing/first-payment/checkout",
            "/v1/app/shell",
        ] {
            assert!(open_while_unpaid(open), "{open}");
        }
        for closed in [
            "/v1/customers",
            "/v1/billingx",
            "/v1/app/shell/x",
            "/v1/authx",
            "/v1/phone/bootstrap",
            "/v1/me",
            "/v1/superuser/companies",
            "/v1/company/setup-checklist",
        ] {
            assert!(!open_while_unpaid(closed), "{closed}");
        }
    }
}
