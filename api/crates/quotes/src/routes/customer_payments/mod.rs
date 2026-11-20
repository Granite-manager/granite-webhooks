//! Customer payments: a company's customers pay for their jobs by card or
//! ACH through the company's own Stripe account (Stripe Connect).
//!
//! - `GET  /v1/company/payment-settings`               → bank account status and who pays each fee
//! - `PUT  /v1/company/payment-settings`               → who pays each fee
//! - `POST /v1/company/payment-settings/bank-account`  → Stripe's page that connects the bank account
//! - `GET  /v1/customers/{id}/payments`                → the payments and payment links of a customer's jobs
//! - `POST /v1/customers/{id}/payments`                → a payment link for an amount of a job
//! - `DELETE /v1/customers/{id}/payments/{payment_id}` → withdraws a link nobody paid
//! - `GET  /v1/public/pay/{payment_id}`                → what the payment page shows
//! - `POST /v1/public/pay/{payment_id}/checkout`       → Stripe's Checkout page for the payment
//! - `POST /v1/public/pay/{payment_id}/demo`           → demo mode only: the payment's outcome
//! - `GET  /v1/public/customer-view/{company_id}/{view_id}/payments` → the customer's own page
//! - `POST /v1/webhooks/stripe-connect`                → Stripe's events about the companies' accounts
//!
//! Stripe's events used: `checkout.session.completed`,
//! `checkout.session.async_payment_succeeded`,
//! `checkout.session.async_payment_failed`, `payment_intent.succeeded`,
//! `payment_intent.processing`, `payment_intent.payment_failed`,
//! `payment_intent.canceled`, `charge.refunded`, `charge.dispute.*`
//! (`created` and `closed` at least), `account.updated` and
//! `account.application.deauthorized`. The Checkout and payment intent events say the same
//! things twice; whichever comes first moves the payment on.
//!
//! Each company gets a Standard connected account, and a payment is a direct
//! charge on it: the money and Stripe's own fee are the company's, and the
//! platform keeps an application fee. The company is quoted one fee per
//! method (3% card, 1% ACH); the application fee is what is left of it after
//! Stripe's usual fee, never below zero.
//!
//! A payment link (`<app>/pay/<payment id>`) never expires: the page asks for
//! a fresh Checkout session, which Stripe keeps for 24 hours. The payment's
//! status follows Stripe's webhooks, and each change is put in the bell of
//! the company's Managers and of whoever made the link.
//!
//! Demo mode (`CUSTOMER_PAYMENTS_DEMO=1`, local runs only) never calls
//! Stripe: the bank account connects at once and the payment page reports
//! the outcome itself.

mod bells;
mod catch_up;
mod fees;
mod links;
mod pay_page;
mod payment;
mod settings;
mod settle;
mod view;
mod webhook;

pub use fees::{FeeSplit, PaymentMethod, split_fee};
pub use links::NewPayment;
pub use pay_page::{DemoOutcome, DemoPayment, PayCheckout, PayPage};
pub use payment::{CustomerPayment, CustomerPaymentStatus};
pub use settings::{
    BankAccount, BankAccountStatus, BankOnboarding, CompanyPaymentSettings, PaymentFeeSetting,
    PaymentFees, PaymentFeesInput,
};
pub use view::{CustomerViewPayments, ViewJob, ViewPayment};
pub use webhook::ConnectWebhookReceived;

use crate::billing::stripe::StripeError;
use crate::error::ApiError;
use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;
use lambda_http::tracing;
use std::sync::Arc;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(settings::payment_settings))
        .routes(routes!(settings::save_payment_fees))
        .routes(routes!(settings::start_bank_onboarding))
        .routes(routes!(links::list_payments))
        .routes(routes!(links::create_payment))
        .routes(routes!(links::cancel_payment))
        .routes(routes!(pay_page::pay_page))
        .routes(routes!(pay_page::pay_checkout))
        .routes(routes!(pay_page::pay_demo))
        .routes(routes!(view::view_payments))
        .routes(routes!(webhook::stripe_connect_webhook))
}

/// The fee a company is quoted on a card payment: 3%.
pub const CARD_RATE_BPS: i32 = 300;
/// The fee a company is quoted on an ACH payment: 1%.
pub const ACH_RATE_BPS: i32 = 100;
/// The least a card payment's fee can be, $9: 3% of $300, the amount under
/// which Stripe's own fee (2.9% + 30¢) is more than 3%.
pub const CARD_MIN_FEE_CENTS: i64 = 900;
/// The smallest card payment, $10: the minimum fee must fit inside it.
const MIN_CARD_AMOUNT_CENTS: i64 = 1_000;
/// The fee sliders move in 0.1% steps.
const SHARE_STEP_BPS: i32 = 10;
/// The smallest payment, $1, and the largest Stripe takes, $999,999.99.
const MIN_AMOUNT_CENTS: i64 = 100;
const MAX_AMOUNT_CENTS: i64 = 99_999_999;
/// `positions.id` of a Manager: they hear about every payment.
const MANAGER_POSITION: i32 = 5;
/// The account a company gets in demo mode: `acct_demo_<company id>`.
const DEMO_ACCOUNT_PREFIX: &str = "acct_demo_";
/// How often Stripe is asked about one payment whose webhook may not have
/// come, in seconds: from its payment page, and from the CRM's list.
const CHECK_FROM_PAGE_SECS: i64 = 60;
const CHECK_FROM_LIST_SECS: i64 = 300;
/// Stripe is asked about this many payments at most while listing.
const CHECK_FROM_LIST_MOST: usize = 3;
/// How often Stripe is asked whether it approved a company's account, in
/// seconds, however many pages ask for the company's payment settings.
const CHECK_ACCOUNT_SECS: i64 = 60;
/// `notifications.message` holds this many characters.
const MESSAGE_CHARS: usize = 255;

/// Customer payments' own settings.
#[derive(Clone)]
pub struct CustomerPayments {
    /// Signs the Connect webhook's events; they are refused without it.
    webhook_secret: Option<Arc<str>>,
    /// Never call Stripe (local runs).
    demo: bool,
}

impl CustomerPayments {
    pub fn new(webhook_secret: &str, demo: bool) -> Self {
        let secret = webhook_secret.trim();
        Self {
            webhook_secret: (!secret.is_empty()).then(|| Arc::from(secret)),
            demo,
        }
    }

    /// `STRIPE_CONNECT_WEBHOOK_SECRET` and `CUSTOMER_PAYMENTS_DEMO=1`. Demo
    /// mode lets anyone with a link mark it paid, so it is only ever on for a
    /// local run: `app_url` on localhost and not in a real Lambda
    /// (`AWS_EXECUTION_ENV`, which local runs do not set). Asked for anywhere
    /// else, it stays off and says so in the log.
    pub fn from_env(app_url: &str) -> Self {
        let asked = std::env::var("CUSTOMER_PAYMENTS_DEMO").is_ok_and(|value| value.trim() == "1");
        let local = is_local_url(app_url) && std::env::var_os("AWS_EXECUTION_ENV").is_none();
        if asked && !local {
            tracing::error!(
                app_url,
                "CUSTOMER_PAYMENTS_DEMO=1 ignored: demo mode is for local runs only"
            );
        }
        Self::new(
            &std::env::var("STRIPE_CONNECT_WEBHOOK_SECRET").unwrap_or_default(),
            asked && local,
        )
    }

    /// The settings `state` was given (`AppState::with_extension`), else the
    /// environment's.
    pub fn of(state: &AppState) -> Arc<Self> {
        state
            .extensions
            .get_or_insert_with(|| Self::from_env(&state.app_url))
    }
}

/// Whether `url` is on this computer: `localhost`, `*.localhost`, `127.0.0.1`
/// or `[::1]`, any port.
fn is_local_url(url: &str) -> bool {
    let url = url.trim().to_ascii_lowercase();
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or_default();
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = if authority.starts_with('[') {
        authority
            .split(']')
            .next()
            .map(|host| &host[1..])
            .unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    host == "localhost" || host.ends_with(".localhost") || host == "127.0.0.1" || host == "::1"
}

impl CustomerPayments {}

fn stripe_error(error: StripeError) -> ApiError {
    match error {
        StripeError::NotConfigured => ApiError::Unavailable("Payments are not set up".into()),
        StripeError::Api(message) => ApiError::Upstream(format!("Stripe: {message}")),
        StripeError::Request(message) => {
            ApiError::Upstream(format!("Stripe could not be reached: {message}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_mode_is_only_for_this_computer() {
        for local in [
            "http://localhost:5173",
            "http://127.0.0.1:3000/x",
            "https://acme.localhost",
            "http://[::1]:5173",
            "HTTP://LOCALHOST",
        ] {
            assert!(is_local_url(local), "{local}");
        }
        for remote in [
            "https://app.granite-manager.com",
            "https://localhost.evil.com",
            "http://10.0.0.5",
            "",
            "localhost",
        ] {
            assert!(!is_local_url(remote), "{remote}");
        }
    }
}
