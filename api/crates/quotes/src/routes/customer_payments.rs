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

use super::customer_view::{no_store, verified_customer};
use crate::app_origin::{app_origin, company_subdomain};
use crate::auth::{EmployeeUser, UserManager};
use crate::billing::stripe::{PaymentCheckout, StripeError, verify_signature};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use chrono::NaiveDateTime;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{AssertSqlSafe, MySqlPool};
use std::sync::Arc;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(payment_settings))
        .routes(routes!(save_payment_fees))
        .routes(routes!(start_bank_onboarding))
        .routes(routes!(list_payments))
        .routes(routes!(create_payment))
        .routes(routes!(cancel_payment))
        .routes(routes!(pay_page))
        .routes(routes!(pay_checkout))
        .routes(routes!(pay_demo))
        .routes(routes!(view_payments))
        .routes(routes!(stripe_connect_webhook))
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

// ---- fees

/// How a customer pays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PaymentMethod {
    /// A credit or debit card: paid at once.
    Card,
    /// A US bank transfer: about 4 business days.
    Ach,
}

impl PaymentMethod {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Card => "card",
            Self::Ach => "ach",
        }
    }

    fn parse(value: &str) -> Self {
        if value == "ach" {
            Self::Ach
        } else {
            Self::Card
        }
    }

    /// How a message names it.
    const fn words(self) -> &'static str {
        match self {
            Self::Card => "card",
            Self::Ach => "ACH",
        }
    }

    const fn rate_bps(self) -> i32 {
        match self {
            Self::Card => CARD_RATE_BPS,
            Self::Ach => ACH_RATE_BPS,
        }
    }

    /// The least its fee can be, in cents.
    const fn min_fee(self) -> i64 {
        match self {
            Self::Card => CARD_MIN_FEE_CENTS,
            Self::Ach => 0,
        }
    }

    /// Stripe's `payment_method_types` value.
    const fn stripe_type(self) -> &'static str {
        match self {
            Self::Card => "card",
            Self::Ach => "us_bank_account",
        }
    }

    /// Stripe's usual fee on a charge of `charged` cents: 2.9% + 30¢ on a
    /// card, 0.8% capped at $5 on ACH.
    const fn stripe_fee(self, charged: i64) -> i64 {
        match self {
            Self::Card => bps_of(charged, 290) + 30,
            Self::Ach => {
                let fee = bps_of(charged, 80);
                if fee > 500 { 500 } else { fee }
            }
        }
    }
}

/// `bps` basis points of `cents`, rounded half up like JavaScript's
/// `Math.round` (the CRM does the same sums for its previews).
const fn bps_of(cents: i64, bps: i64) -> i64 {
    (cents * bps + 5_000) / 10_000
}

/// A payment's fee and who pays it, in cents.
#[derive(Debug, PartialEq, Eq)]
pub struct FeeSplit {
    /// The whole fee the company is quoted.
    pub fee: i64,
    /// The part added to what the customer pays.
    pub customer_fee: i64,
    /// The part taken from what the company receives.
    pub company_fee: i64,
    /// What the platform keeps: the fee less Stripe's usual fee, never below
    /// zero (Stripe charges its own fee to the company).
    pub application_fee: i64,
}

/// Splits the fee on `amount` cents. The customer's part is rounded first
/// and the company pays the rest, so the two always add up to the fee.
pub fn split_fee(method: PaymentMethod, amount: i64, customer_share_bps: i32) -> FeeSplit {
    let rate = method.rate_bps();
    let share = customer_share_bps.clamp(0, rate);
    let fee = bps_of(amount, i64::from(rate)).max(method.min_fee());
    let customer_fee = bps_of(amount, i64::from(share)).min(fee);
    let charged = amount + customer_fee;
    FeeSplit {
        fee,
        customer_fee,
        company_fee: fee - customer_fee,
        application_fee: (fee - method.stripe_fee(charged)).max(0),
    }
}

/// `500` → `$5`, `750` → `$7.50`, `100000` → `$1,000`.
fn dollars(cents: i64) -> String {
    let whole = (cents / 100).to_string();
    let mut grouped = String::new();
    for (index, digit) in whole.chars().enumerate() {
        if index > 0 && (whole.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    match cents % 100 {
        0 => format!("${grouped}"),
        rest => format!("${grouped}.{rest:02}"),
    }
}

/// `50` → `0.5%`, `100` → `1%`, `5` → `0.05%`.
fn percent(bps: i32) -> String {
    let (whole, rest) = (bps / 100, bps % 100);
    if rest == 0 {
        format!("{whole}%")
    } else {
        let rest = format!("{rest:02}");
        format!("{whole}.{}%", rest.trim_end_matches('0'))
    }
}

/// The name of the fee's line on Stripe's page: `Processing fee (0.5%)`.
fn fee_line_name(customer_fee_bps: i32) -> String {
    if customer_fee_bps > 0 {
        format!("Processing fee ({})", percent(customer_fee_bps))
    } else {
        "Processing fee".into()
    }
}

// ---- the company's settings

/// Whether the company can be paid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BankAccountStatus {
    /// Nobody started connecting a bank account.
    NotConnected,
    /// Connecting was started; Stripe has not approved the account yet (the
    /// form is unfinished or Stripe is checking it).
    PendingVerification,
    /// The company can take payments.
    Connected,
}

/// The company's bank account, as Stripe last reported it.
#[derive(Debug, Serialize, ToSchema)]
pub struct BankAccount {
    /// Whether the company can be paid.
    pub status: BankAccountStatus,
    /// The name of the bank payouts go to (the account's default bank
    /// account at Stripe); null until connected, or when Stripe does not say.
    pub bank_name: Option<String>,
    /// The account number's last 4 digits; null like `bank_name`.
    pub last4: Option<String>,
}

/// How one payment method's fee is split.
#[derive(Debug, Serialize, ToSchema)]
pub struct PaymentFeeSetting {
    /// The fee on a payment, in basis points (300 = 3%).
    pub rate_bps: i32,
    /// The customer's part of it, `0..=rate_bps`; the company pays the rest.
    pub customer_share_bps: i32,
    /// The least the fee on one payment can be, in US cents (900 = $9 for
    /// cards, 0 for ACH): a small payment's fee is this, not the rate.
    pub min_fee_cents: i64,
    /// The split cannot be changed: the company pays the whole fee. True for
    /// cards for now.
    pub locked: bool,
}

/// Each method's fee.
#[derive(Debug, Serialize, ToSchema)]
pub struct PaymentFees {
    /// Card payments.
    pub card: PaymentFeeSetting,
    /// ACH bank transfers.
    pub ach: PaymentFeeSetting,
}

/// A company's customer payment settings.
#[derive(Debug, Serialize, ToSchema)]
pub struct CompanyPaymentSettings {
    /// The bank account customers' money goes to.
    pub bank: BankAccount,
    /// The fee on each payment method and who pays it.
    pub fees: PaymentFees,
    /// Customer payments are turned on for the company (by the platform,
    /// company by company). While false nothing can be set up and no link
    /// can be made (403); links made before stay payable.
    pub enabled: bool,
}

#[derive(Default, sqlx::FromRow)]
struct SettingsRow {
    enabled: bool,
    stripe_account_id: Option<String>,
    charges_enabled: bool,
    bank_name: Option<String>,
    bank_last4: Option<String>,
    ach_customer_share_bps: i32,
}

impl SettingsRow {
    /// The company's connected account. A demo account counts only in demo
    /// mode, so one left in a database never reaches Stripe.
    fn account(&self, demo: bool) -> Option<&str> {
        self.stripe_account_id
            .as_deref()
            .filter(|account| !account.is_empty())
            .filter(|account| demo || !account.starts_with(DEMO_ACCOUNT_PREFIX))
    }

    fn status(&self, demo: bool) -> BankAccountStatus {
        match self.account(demo) {
            None => BankAccountStatus::NotConnected,
            Some(_) if self.charges_enabled => BankAccountStatus::Connected,
            Some(_) => BankAccountStatus::PendingVerification,
        }
    }

    const fn share_bps(&self, method: PaymentMethod) -> i32 {
        match method {
            // Locked: see `PaymentFeeSetting::locked`.
            PaymentMethod::Card => 0,
            PaymentMethod::Ach => self.ach_customer_share_bps,
        }
    }
}

async fn settings_row(pool: &MySqlPool, company_id: i32) -> sqlx::Result<SettingsRow> {
    let row: Option<SettingsRow> = sqlx::query_as(
        "SELECT c.customer_payments_enabled <> 0 AS enabled, s.stripe_account_id,
                COALESCE(s.charges_enabled, 0) <> 0 AS charges_enabled,
                s.bank_name, s.bank_last4,
                COALESCE(s.ach_customer_share_bps, 0) AS ach_customer_share_bps
           FROM company c
           LEFT JOIN company_payment_settings s ON s.company_id = c.id
          WHERE c.id = ?",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.unwrap_or_default())
}

/// 403 unless customer payments are turned on for the company.
fn turned_on(settings: &SettingsRow) -> ApiResult<()> {
    if settings.enabled {
        Ok(())
    } else {
        Err(ApiError::Denied(
            "Customer payments are not turned on for this company".into(),
        ))
    }
}

/// Saves what Stripe says about a connected account (an `account` object).
async fn save_account(pool: &MySqlPool, account: &Value) -> sqlx::Result<()> {
    let Some(id) = account["id"].as_str() else {
        return Ok(());
    };
    // The bank account payouts go to: the one Stripe marks as the default
    // for its currency, else the first. A company that adds a second bank
    // account and makes it the default must see that one, not the old.
    let banks = account["external_accounts"]["data"].as_array();
    let bank = banks
        .and_then(|banks| {
            banks
                .iter()
                .find(|bank| bank["default_for_currency"] == true)
                .or_else(|| banks.first())
        })
        .unwrap_or(&Value::Null);
    sqlx::query(
        "UPDATE company_payment_settings
            SET charges_enabled = ?, bank_name = COALESCE(?, bank_name),
                bank_last4 = COALESCE(?, bank_last4)
          WHERE stripe_account_id = ?",
    )
    .bind(account["charges_enabled"].as_bool() == Some(true))
    .bind(bank["bank_name"].as_str())
    .bind(bank["last4"].as_str())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// The CRM's address for the company (its subdomain when it has one).
async fn company_origin(state: &AppState, company_id: i32) -> sqlx::Result<String> {
    let subdomain = company_subdomain(&state.pool, company_id).await?;
    Ok(app_origin(&state.app_url, subdomain.as_deref())
        .trim_end_matches('/')
        .to_string())
}

/// Show the company's customer payment settings
///
/// Whether the company's bank account is connected (so customers can pay
/// it), and for each payment method the fee and how much of it the company
/// passes on to the customer. While the account is being verified, Stripe is
/// asked whether it approved it at most once a minute per company; between
/// those the answer is the last one kept. Connect an account with `POST
/// /v1/company/payment-settings/bank-account`; make payment links with `POST
/// /v1/customers/{id}/payments`.
#[crate::openapi::path(
    get,
    path = "/v1/company/payment-settings",
    operation_id = "get_company_payment_settings",
    tag = "customer-payments",
    responses(
        (status = 200, description = "The settings", body = CompanyPaymentSettings),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn payment_settings(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<CompanyPaymentSettings>> {
    let config = CustomerPayments::of(&state);
    let mut row = settings_row(&state.pool, user.company_id).await?;
    let unverified = row
        .account(config.demo)
        .filter(|_| {
            row.enabled && !config.demo && !row.charges_enabled && state.stripe.is_enabled()
        })
        .map(str::to_string);
    // One asker a minute for the whole company, whoever opens whichever
    // page: the others answer from what is kept, and Stripe's own
    // `account.updated` event says when the account is approved.
    let mine = match &unverified {
        Some(_) => {
            sqlx::query(
                "UPDATE company_payment_settings SET stripe_checked_at = UTC_TIMESTAMP()
                  WHERE company_id = ?
                    AND (stripe_checked_at IS NULL
                         OR stripe_checked_at <= UTC_TIMESTAMP() - INTERVAL ? SECOND)",
            )
            .bind(user.company_id)
            .bind(CHECK_ACCOUNT_SECS)
            .execute(&state.pool)
            .await?
            .rows_affected()
                == 1
        }
        None => false,
    };
    if let Some(account) = unverified.filter(|_| mine) {
        // Stripe's answer only refreshes what is kept: the page still opens
        // when Stripe is down.
        match state.stripe.account(&account).await {
            Ok(account) => {
                save_account(&state.pool, &account).await?;
                row = settings_row(&state.pool, user.company_id).await?;
            }
            Err(error) => {
                tracing::warn!(%error, company_id = user.company_id, "Stripe account not read");
            }
        }
    }
    let status = row.status(config.demo);
    let connected = status != BankAccountStatus::NotConnected;
    Ok(Json(CompanyPaymentSettings {
        enabled: row.enabled,
        fees: PaymentFees {
            card: PaymentFeeSetting {
                rate_bps: CARD_RATE_BPS,
                customer_share_bps: row.share_bps(PaymentMethod::Card),
                min_fee_cents: PaymentMethod::Card.min_fee(),
                locked: true,
            },
            ach: PaymentFeeSetting {
                rate_bps: ACH_RATE_BPS,
                customer_share_bps: row.share_bps(PaymentMethod::Ach),
                min_fee_cents: PaymentMethod::Ach.min_fee(),
                locked: false,
            },
        },
        bank: BankAccount {
            status,
            bank_name: row.bank_name.filter(|_| connected),
            last4: row.bank_last4.filter(|_| connected),
        },
    }))
}

/// Body of `PUT /v1/company/payment-settings`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PaymentFeesInput {
    /// The customer's part of the 3% card fee, in basis points. Must be 0:
    /// the card fee cannot be passed on yet.
    pub card_customer_share_bps: i32,
    /// The customer's part of the 1% ACH fee, in basis points: 0 to 100 in
    /// steps of 10 (0.1%). The company pays the rest.
    pub ach_customer_share_bps: i32,
}

/// Set who pays the payment fees
///
/// How much of each method's fee the company adds to what its customers pay
/// (the company pays the rest). Payment links made afterwards use the new
/// split; existing ones keep theirs. Superusers and admins who are Managers
/// only.
#[crate::openapi::path(
    put,
    path = "/v1/company/payment-settings",
    operation_id = "set_company_payment_fees",
    tag = "customer-payments",
    request_body = PaymentFeesInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "A share is out of range", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser or an admin Manager, or customer payments are not turned on for the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn save_payment_fees(
    State(state): State<AppState>,
    UserManager(user): UserManager,
    JsonBody(input): JsonBody<PaymentFeesInput>,
) -> ApiResult<StatusCode> {
    if input.card_customer_share_bps != 0 {
        return Err(ApiError::BadRequest(
            "The card fee cannot be passed on to customers yet".into(),
        ));
    }
    turned_on(&settings_row(&state.pool, user.company_id).await?)?;
    let ach = input.ach_customer_share_bps;
    if !(0..=ACH_RATE_BPS).contains(&ach) || ach % SHARE_STEP_BPS != 0 {
        return Err(ApiError::BadRequest(
            "The customer's part of the ACH fee must be 0% to 1%, in steps of 0.1%".into(),
        ));
    }
    sqlx::query(
        "INSERT INTO company_payment_settings
                (company_id, card_customer_share_bps, ach_customer_share_bps)
         VALUES (?, 0, ?)
         ON DUPLICATE KEY UPDATE card_customer_share_bps = 0,
                                 ach_customer_share_bps = VALUES(ach_customer_share_bps)",
    )
    .bind(user.company_id)
    .bind(ach)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Where the bank account is connected.
#[derive(Debug, Serialize, ToSchema)]
pub struct BankOnboarding {
    /// Stripe's page to open: the company fills in its business and bank
    /// details there and is sent back to Admin → Company. The link works once
    /// and for a few minutes. Null in demo mode, where the account is
    /// connected at once.
    pub url: Option<String>,
}

/// Connect the company's bank account
///
/// Starts, or continues, Stripe's onboarding of the company: the first call
/// creates its Stripe account. Open `url`; afterwards `GET
/// /v1/company/payment-settings` says `pending_verification` until Stripe
/// approves the account, then `connected`. Superusers and admins who are
/// Managers only.
#[crate::openapi::path(
    post,
    path = "/v1/company/payment-settings/bank-account",
    operation_id = "start_company_bank_onboarding",
    tag = "customer-payments",
    responses(
        (status = 200, description = "Stripe's page", body = BankOnboarding),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser or an admin Manager, or customer payments are not turned on for the company", body = ErrorBody),
        (status = 404, description = "The company was deleted", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Stripe failed", body = ErrorBody),
        (status = 503, description = "Payments are not set up", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn start_bank_onboarding(
    State(state): State<AppState>,
    UserManager(user): UserManager,
) -> ApiResult<Json<BankOnboarding>> {
    let config = CustomerPayments::of(&state);
    let company_id = user.company_id;
    let row = settings_row(&state.pool, company_id).await?;
    turned_on(&row)?;
    if config.demo {
        sqlx::query(
            "INSERT INTO company_payment_settings
                    (company_id, stripe_account_id, charges_enabled, bank_name, bank_last4)
             VALUES (?, ?, 1, 'Demo Bank', '4242')
             ON DUPLICATE KEY UPDATE stripe_account_id = VALUES(stripe_account_id),
                                     charges_enabled = 1, bank_name = 'Demo Bank',
                                     bank_last4 = '4242'",
        )
        .bind(company_id)
        .bind(format!("{DEMO_ACCOUNT_PREFIX}{company_id}"))
        .execute(&state.pool)
        .await?;
        return Ok(Json(BankOnboarding { url: None }));
    }
    if !state.stripe.is_enabled() {
        return Err(ApiError::Unavailable("Payments are not set up".into()));
    }
    let account = if let Some(account) = row.account(false) {
        account.to_string()
    } else {
        let name: Option<String> = sqlx::query_scalar(
            "SELECT COALESCE(NULLIF(TRIM(name), ''), CONCAT('Company ', id))
               FROM company WHERE id = ? AND deleted_at IS NULL",
        )
        .bind(company_id)
        .fetch_optional(&state.pool)
        .await?;
        let name = name.ok_or(ApiError::NotFound)?;
        let created = state
            .stripe
            .create_connected_account(&name, company_id)
            .await
            .map_err(stripe_error)?;
        let account = created["id"]
            .as_str()
            .ok_or_else(|| ApiError::Upstream("Stripe returned no account id".into()))?
            .to_string();
        sqlx::query(
            "INSERT INTO company_payment_settings (company_id, stripe_account_id)
             VALUES (?, ?)
             ON DUPLICATE KEY UPDATE stripe_account_id = VALUES(stripe_account_id),
                                     charges_enabled = 0, bank_name = NULL,
                                     bank_last4 = NULL",
        )
        .bind(company_id)
        .bind(&account)
        .execute(&state.pool)
        .await?;
        account
    };
    // Back from Stripe's page, the first look at the settings asks Stripe
    // at once instead of waiting out the minute.
    sqlx::query(
        "UPDATE company_payment_settings SET stripe_checked_at = NULL WHERE company_id = ?",
    )
    .bind(company_id)
    .execute(&state.pool)
    .await?;
    let page = format!(
        "{}/admin/company",
        company_origin(&state, company_id).await?
    );
    let link = state
        .stripe
        .create_account_link(
            &account,
            &format!("{page}?payments=refresh"),
            &format!("{page}?payments=return"),
        )
        .await
        .map_err(stripe_error)?;
    let url = link["url"]
        .as_str()
        .ok_or_else(|| ApiError::Upstream("Stripe returned no onboarding link".into()))?;
    Ok(Json(BankOnboarding {
        url: Some(url.into()),
    }))
}

// ---- payments

/// Where a payment is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum CustomerPaymentStatus {
    /// The link exists and nobody paid yet.
    Created,
    /// The company withdrew the link before anybody paid.
    Canceled,
    /// An ACH transfer is on its way (about 4 business days).
    Pending,
    /// The money reached the company.
    Paid,
    /// The ACH transfer did not go through; `failure_reason` says why.
    Failed,
    /// The company gave the whole payment back.
    Refunded,
    /// The customer disputed the payment with their bank, which has not
    /// decided yet. The company still holds the money, so it counts as paid
    /// toward the job. Won, it is `paid` again; lost, it is `lost`.
    Disputed,
    /// The bank decided the dispute for the customer and took the money
    /// back: nothing of it counts toward the job any more. Final.
    Lost,
}

impl CustomerPaymentStatus {
    fn parse(value: &str) -> Self {
        match value {
            "canceled" => Self::Canceled,
            "pending" => Self::Pending,
            "paid" => Self::Paid,
            "failed" => Self::Failed,
            "refunded" => Self::Refunded,
            "disputed" => Self::Disputed,
            "lost" => Self::Lost,
            _ => Self::Created,
        }
    }

    /// The company holds its money: paid, or disputed and not decided yet.
    const fn holds_money(self) -> bool {
        matches!(self, Self::Paid | Self::Disputed)
    }
}

/// One payment, or payment link, of a customer's job.
#[derive(Debug, Serialize, ToSchema)]
pub struct CustomerPayment {
    /// The payment's id (`pay_...`), also the last part of its link.
    pub id: String,
    /// The job it is for, as `GET /v1/customers/{id}/projects` lists them:
    /// `deal-<deal id>`, or `sale-<sale id>` for a sale that is not part of
    /// a deal.
    pub project_key: String,
    /// How the customer pays.
    pub method: PaymentMethod,
    /// Where the payment is.
    pub status: CustomerPaymentStatus,
    /// What goes toward the job, in US cents.
    pub amount_cents: i32,
    /// The whole fee on it, in US cents, as split when the link was made.
    pub fee_cents: i32,
    /// The part of the fee added to what the customer pays.
    pub customer_fee_cents: i32,
    /// The customer's share of the fee in basis points (50 = 0.5%), as set
    /// when the link was made.
    pub customer_fee_bps: i32,
    /// The part of the fee taken from what the company receives.
    pub company_fee_cents: i32,
    /// What was given back of what the customer was charged, in US cents: 0
    /// unless refunded. Above 0 on a `paid` payment, it was refunded in part.
    pub refunded_cents: i32,
    /// The page the customer pays on. It never expires.
    pub url: String,
    /// Why it failed, as a short plain phrase of our own, never Stripe's
    /// text ("insufficient funds", "the bank account is closed", "the bank
    /// declined the transfer"); null unless `failed`.
    pub failure_reason: Option<String>,
    /// When the link was made (ISO 8601, UTC).
    pub created_at: String,
}

#[derive(sqlx::FromRow)]
struct PaymentRow {
    id: u64,
    public_id: String,
    company_id: i32,
    customer_id: i32,
    deal_id: Option<i64>,
    sale_id: Option<i32>,
    method: String,
    status: String,
    amount_cents: i32,
    fee_cents: i32,
    customer_fee_cents: i32,
    customer_fee_bps: i32,
    company_fee_cents: i32,
    application_fee_cents: i32,
    refunded_cents: i32,
    stripe_account_id: String,
    stripe_session_id: Option<String>,
    stripe_checked_at: Option<NaiveDateTime>,
    failure_reason: Option<String>,
    created_by: Option<i32>,
    created_at: NaiveDateTime,
    customer_name: Option<String>,
    project_name: Option<String>,
    /// The Stripe account the company uses today; `stripe_account_id` is the
    /// one the payment was made for.
    company_account: Option<String>,
}

/// The job a payment `p` shows under, for a `FROM customer_payments p`: its
/// sale `ps` and its deal `pd`. A payment made for a sale moves to the
/// sale's deal once the sale is part of one, as the job itself does in `GET
/// /v1/customers/{id}/projects`; [`PAYMENT_DEAL`] is that deal. Every reader
/// of a payment's job uses these, so the list and the balance agree.
macro_rules! payment_job_joins {
    () => {
        "LEFT JOIN sales ps ON ps.id = p.sale_id
         LEFT JOIN deals pd ON pd.id = COALESCE(p.deal_id, ps.deal_id) AND pd.deleted_at IS NULL"
    };
}

/// The deal a payment shows under (see `payment_job_joins`); NULL for a
/// payment of a sale that is not part of a deal.
macro_rules! payment_deal {
    () => {
        "COALESCE(p.deal_id, pd.id)"
    };
}

/// Every payment column, with the job it shows under.
const PAYMENT_SELECT: &str = concat!(
    "SELECT p.id, p.public_id, p.company_id, p.customer_id,
        CAST(",
    payment_deal!(),
    " AS SIGNED) AS deal_id, p.sale_id,
        CAST(p.method AS CHAR) AS method, CAST(p.status AS CHAR) AS status,
        p.amount_cents, p.fee_cents, p.customer_fee_cents, p.customer_fee_bps,
        p.company_fee_cents, p.application_fee_cents, p.refunded_cents,
        p.stripe_account_id, p.stripe_session_id, p.stripe_checked_at,
        p.failure_reason, p.created_by, p.created_at,
        NULLIF(TRIM(pc.name), '') AS customer_name,
        COALESCE(NULLIF(TRIM(pd.title), ''), NULLIF(TRIM(ps.project_address), ''))
          AS project_name,
        pcs.stripe_account_id AS company_account
   FROM customer_payments p
   LEFT JOIN customers pc ON pc.id = p.customer_id
   LEFT JOIN company_payment_settings pcs ON pcs.company_id = p.company_id
   ",
    payment_job_joins!()
);

impl PaymentRow {
    fn method(&self) -> PaymentMethod {
        PaymentMethod::parse(&self.method)
    }

    fn status(&self) -> CustomerPaymentStatus {
        CustomerPaymentStatus::parse(&self.status)
    }

    /// The job's number as people say it: "Project #10226" (a deal) or
    /// "Sale #812" (a sale outside any deal).
    fn project_number(&self) -> String {
        match (self.deal_id, self.sale_id) {
            (Some(deal), _) => format!("Project #{deal}"),
            (None, sale) => format!("Sale #{}", sale.unwrap_or_default()),
        }
    }

    /// What the customer sees on Stripe's page and in their statement: the
    /// job's name with its number ("12 Oak St · Project #10226"), or the
    /// number alone when the job has no name.
    fn checkout_name(&self) -> String {
        let number = self.project_number();
        self.project_name
            .as_ref()
            .map_or_else(|| number.clone(), |name| format!("{name} · {number}"))
    }

    fn project_key(&self) -> String {
        match (self.deal_id, self.sale_id) {
            (Some(deal), _) => format!("deal-{deal}"),
            (None, sale) => format!("sale-{}", sale.unwrap_or_default()),
        }
    }

    fn url(&self, origin: &str) -> String {
        format!("{origin}/pay/{}", self.public_id)
    }

    fn created_at(&self) -> String {
        self.created_at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
    }

    /// What it put toward the job: its amount while the company holds the
    /// money (paid, or disputed and not decided yet), less what was given
    /// back (a refund takes the customer's fee back last).
    fn paid_toward_job(&self) -> i64 {
        if self.status().holds_money() {
            i64::from(self.amount_cents - self.refunded_cents.min(self.amount_cents))
        } else {
            0
        }
    }

    /// An unpaid link made for a Stripe account the company no longer uses
    /// (the account was changed or removed). A link charges the account it
    /// was made for, so paying this one would send the money to the old
    /// account: it is withdrawn instead (`withdraw_stale_links`).
    fn of_another_account(&self) -> bool {
        self.status() == CustomerPaymentStatus::Created
            && self.company_account.as_deref().unwrap_or_default() != self.stripe_account_id
    }

    /// Whether Stripe may know more than we do: the payment waits (`created`
    /// or `pending`), the customer opened Stripe's page for it, and Stripe
    /// was not asked in the last `every_secs` seconds.
    fn may_be_behind(&self, every_secs: i64) -> bool {
        matches!(
            self.status(),
            CustomerPaymentStatus::Created | CustomerPaymentStatus::Pending
        ) && self.stripe_session_id.is_some()
            && !self.stripe_account_id.starts_with(DEMO_ACCOUNT_PREFIX)
            && self
                .stripe_checked_at
                .is_none_or(|at| (chrono::Utc::now().naive_utc() - at).num_seconds() >= every_secs)
    }

    fn into_payment(self, origin: &str) -> CustomerPayment {
        CustomerPayment {
            project_key: self.project_key(),
            method: self.method(),
            status: self.status(),
            url: self.url(origin),
            created_at: self.created_at(),
            id: self.public_id,
            amount_cents: self.amount_cents,
            fee_cents: self.fee_cents,
            customer_fee_cents: self.customer_fee_cents,
            customer_fee_bps: self.customer_fee_bps,
            company_fee_cents: self.company_fee_cents,
            refunded_cents: self.refunded_cents,
            failure_reason: self.failure_reason,
        }
    }
}

async fn payment_by_id<'e>(
    executor: impl sqlx::MySqlExecutor<'e>,
    public_id: &str,
) -> sqlx::Result<Option<PaymentRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "{PAYMENT_SELECT} WHERE p.public_id = ?"
    )))
    .bind(public_id)
    .fetch_optional(executor)
    .await
}

/// Asks Stripe where a waiting payment is, for when its webhook did not
/// come (the server was down, the endpoint misconfigured): the payment's
/// Checkout session, and for a bank transfer its payment intent. What Stripe
/// says moves the payment on exactly as the webhook would, bells included; a
/// webhook that comes later changes nothing. At most once per `every_secs`
/// for one payment, whoever asks. True when the payment changed. Stripe not
/// answering is only logged: the caller shows what is kept.
async fn catch_up(state: &AppState, payment: &PaymentRow, every_secs: i64) -> sqlx::Result<bool> {
    if CustomerPayments::of(state).demo
        || !state.stripe.is_enabled()
        || !payment.may_be_behind(every_secs)
    {
        return Ok(false);
    }
    let Some(session) = payment.stripe_session_id.as_deref() else {
        return Ok(false);
    };
    let pool = &state.pool;
    // One asker at a time: the others see the time just written.
    let mine = sqlx::query(
        "UPDATE customer_payments SET stripe_checked_at = UTC_TIMESTAMP()
          WHERE public_id = ? AND status IN ('created', 'pending')
            AND (stripe_checked_at IS NULL
                 OR stripe_checked_at <= UTC_TIMESTAMP() - INTERVAL ? SECOND)",
    )
    .bind(&payment.public_id)
    .bind(every_secs)
    .execute(pool)
    .await?
    .rows_affected()
        == 1;
    if !mine {
        return Ok(false);
    }
    let account = payment.stripe_account_id.as_str();
    let id = payment.public_id.as_str();
    let session = match state.stripe.checkout_session(account, session).await {
        Ok(session) => session,
        Err(error) => {
            tracing::warn!(%error, payment = id, "Checkout session not read");
            return Ok(false);
        }
    };
    // Open or expired: nobody paid on it.
    if session["status"] != "complete" {
        return Ok(false);
    }
    let intent = id_of(&session["payment_intent"]);
    if session["payment_status"] == "paid" {
        return settle(pool, id, Some(account), intent, Outcome::Paid).await;
    }
    // A bank transfer: on its way, arrived or bounced.
    let Some(intent) = intent else {
        return Ok(false);
    };
    let found = match state.stripe.payment_intent(account, intent).await {
        Ok(found) => found,
        Err(error) => {
            tracing::warn!(%error, payment = id, "payment intent not read");
            return Ok(false);
        }
    };
    let Some(outcome) = transfer_outcome(payment.method(), &found) else {
        return Ok(false);
    };
    settle(pool, id, Some(account), Some(intent), outcome).await
}

/// What the payment intent of a finished Checkout page says of a payment
/// that was not paid at once (a bank transfer). None while there is nothing
/// to record: with `requires_action` the customer typed their bank account
/// by hand and has yet to confirm it (micro-deposits), so no money has left
/// their account and the payment is not on its way.
fn transfer_outcome(method: PaymentMethod, intent: &Value) -> Option<Outcome> {
    match intent["status"].as_str() {
        Some("succeeded") => Some(Outcome::Paid),
        Some("processing") => Some(Outcome::Pending),
        // The transfer bounced or was called off, whether or not we heard
        // that it started.
        Some("requires_payment_method" | "canceled") => {
            Some(Outcome::Failed(transfer_failure(method, intent)))
        }
        _ => None,
    }
}

/// Why a payment intent's transfer did not go through, in plain words: the
/// bank's reason (`decline_reason`), or that it was called off when Stripe
/// canceled the intent without one (a bank account never confirmed).
fn transfer_failure(method: PaymentMethod, intent: &Value) -> String {
    let error = &intent["last_payment_error"];
    if intent["status"] == "canceled" && error.is_null() {
        "the transfer was canceled before it started".into()
    } else {
        decline_reason(method, error)
    }
}

/// Withdraws the company's unpaid links that were made for a Stripe account
/// it no longer uses (its account was changed or removed), and closes their
/// open Checkout pages as far as Stripe lets us. A link charges the account
/// it was made for, so such a link would send the customer's money to the
/// old account. Payments that were made or are on their way keep their
/// account, whose events still move them (refunds, disputes, a transfer
/// that clears); a payment that still arrives on a page left open is
/// recorded the same way. Returns how many links were withdrawn.
async fn withdraw_stale_links(state: &AppState, company_id: i32) -> sqlx::Result<u64> {
    const STALE: &str = "p.company_id = ? AND p.status = 'created'
        AND p.stripe_account_id <> COALESCE(s.stripe_account_id, '')";
    let stale: Vec<(String, String, Option<String>)> = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT p.public_id, p.stripe_account_id, p.stripe_session_id
           FROM customer_payments p
           LEFT JOIN company_payment_settings s ON s.company_id = p.company_id
          WHERE {STALE}"
    )))
    .bind(company_id)
    .fetch_all(&state.pool)
    .await?;
    let mut withdrawn = 0;
    for (id, account, session) in stale {
        let changed = sqlx::query(AssertSqlSafe(format!(
            "UPDATE customer_payments p
               LEFT JOIN company_payment_settings s ON s.company_id = p.company_id
                SET p.status = 'canceled'
              WHERE p.public_id = ? AND {STALE}"
        )))
        .bind(&id)
        .bind(company_id)
        .execute(&state.pool)
        .await?
        .rows_affected();
        if changed == 0 {
            continue;
        }
        withdrawn += changed;
        tracing::warn!(
            payment = %id,
            company_id,
            account = %account,
            "link of a Stripe account the company no longer uses: withdrawn"
        );
        let on_stripe = account.starts_with("acct_")
            && !account.starts_with(DEMO_ACCOUNT_PREFIX)
            && state.stripe.is_enabled();
        if let Some(session) = session.filter(|_| on_stripe)
            && let Err(error) = state
                .stripe
                .expire_checkout_session(&account, &session)
                .await
        {
            tracing::warn!(%error, payment = %id, "Checkout session of a withdrawn link not closed");
        }
    }
    Ok(withdrawn)
}

/// The head of the family `customer_id` belongs to (its parent, else
/// itself): the Projects tab shows the whole family's jobs. 404 when the
/// customer is not the company's.
async fn family_of(pool: &MySqlPool, company_id: i32, customer_id: i32) -> ApiResult<i32> {
    let parent: Option<Option<i32>> =
        sqlx::query_scalar("SELECT parent_id FROM customers WHERE id = ? AND company_id = ?")
            .bind(customer_id)
            .bind(company_id)
            .fetch_optional(pool)
            .await?;
    let parent = parent.ok_or(ApiError::NotFound)?;
    Ok(parent.filter(|parent| *parent > 0).unwrap_or(customer_id))
}

/// The customers of a family, for an `IN (...)`.
const FAMILY_SQL: &str = "SELECT fc.id FROM customers fc
     WHERE fc.company_id = ? AND (fc.id = ? OR fc.parent_id = ?)";

/// List a customer's payments
///
/// The payments and unpaid payment links of the jobs of the customer's
/// family (the customer, its parent and the parent's other projects: what
/// `GET /v1/customers/{id}/projects` lists), newest first, withdrawn links
/// included. `project_key` names the job. Money that reached the company is
/// `paid`; an ACH transfer on its way is `pending`; a `disputed` payment
/// still counts toward its job, a `lost` one no longer does. A waiting payment whose
/// news from Stripe may have been missed is checked with Stripe here, at
/// most once in 5 minutes each. Unpaid links made for a Stripe account the
/// company no longer uses are withdrawn here (`canceled`).
#[crate::openapi::path(
    get,
    path = "/v1/customers/{id}/payments",
    operation_id = "list_customer_payments",
    tag = "customer-payments",
    params(("id" = i32, Path, description = "Customer id")),
    responses(
        (status = 200, description = "The payments, newest first", body = [CustomerPayment]),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such customer in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_payments(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<Vec<CustomerPayment>>> {
    let head = family_of(&state.pool, user.company_id, id).await?;
    let list = || async {
        sqlx::query_as::<_, PaymentRow>(AssertSqlSafe(format!(
            "{PAYMENT_SELECT}
              WHERE p.company_id = ? AND p.customer_id IN ({FAMILY_SQL})
              ORDER BY p.created_at DESC, p.id DESC"
        )))
        .bind(user.company_id)
        .bind(user.company_id)
        .bind(head)
        .bind(head)
        .fetch_all(&state.pool)
        .await
    };
    let mut rows = list().await?;
    // Links made for a Stripe account the company no longer uses are
    // withdrawn: nobody may pay into the old account.
    if rows.iter().any(PaymentRow::of_another_account) {
        withdraw_stale_links(&state, user.company_id).await?;
        rows = list().await?;
    }
    // A webhook that never came: Stripe is asked about the waiting payments
    // (a few, each seldom), and the list is read again when one moved.
    let mut moved = false;
    for row in rows
        .iter()
        .filter(|row| row.may_be_behind(CHECK_FROM_LIST_SECS))
        .take(CHECK_FROM_LIST_MOST)
    {
        moved |= catch_up(&state, row, CHECK_FROM_LIST_SECS).await?;
    }
    if moved {
        rows = list().await?;
    }
    let origin = company_origin(&state, user.company_id).await?;
    Ok(Json(
        rows.into_iter()
            .map(|row| row.into_payment(&origin))
            .collect(),
    ))
}

/// Body of `POST /v1/customers/{id}/payments`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewPayment {
    /// The job to pay for, from `GET /v1/customers/{id}/projects`:
    /// `deal-<deal id>`, or `sale-<sale id>` for a sale that is not part of
    /// a deal.
    pub project_key: String,
    /// How the customer will pay.
    pub method: PaymentMethod,
    /// What goes toward the job, in US cents: 100 ($1) to 99999999
    /// ($999,999.99); a card payment at least 1000 ($10). The customer's part
    /// of the fee is added on top.
    pub amount_cents: i64,
}

/// The job a `project_key` names.
enum Job {
    Deal(i64),
    Sale(i32),
}

fn parse_project_key(key: &str) -> Option<Job> {
    let (kind, id) = key.trim().split_once('-')?;
    if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    match kind {
        "deal" => id.parse().ok().map(Job::Deal),
        "sale" => id.parse().ok().map(Job::Sale),
        _ => None,
    }
}

/// `pay_` and 32 hex characters: 128 random bits, so a link cannot be guessed.
fn new_public_id() -> ApiResult<String> {
    use std::fmt::Write as _;
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|err| ApiError::Internal(err.to_string()))?;
    Ok(bytes.iter().fold(String::from("pay_"), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    }))
}

/// Make a payment link
///
/// A link the customer pays `amount_cents` of one job on, by card or ACH.
/// The fee split in effect now is saved on the payment. Send `url` to the
/// customer: it never expires, and the payment stays `created` until they
/// pay. The amount cannot be more than is left to pay on the job: its price
/// less what was paid (a disputed payment counts until the bank decides),
/// what is on its way and the links not paid yet (400 says how much that
/// is). A deal's payments include those made for its sales before they
/// joined it. A sale that is part of a deal takes no link of its own (400:
/// make it for the deal), nor does a cancelled sale. 409 until the company's bank account is connected (`GET
/// /v1/company/payment-settings`). Withdraw an unpaid link with `DELETE
/// /v1/customers/{id}/payments/{payment_id}`.
#[crate::openapi::path(
    post,
    path = "/v1/customers/{id}/payments",
    operation_id = "create_customer_payment",
    tag = "customer-payments",
    params(("id" = i32, Path, description = "Customer id")),
    request_body = NewPayment,
    responses(
        (status = 201, description = "The payment link", body = CustomerPayment),
        (status = 400, description = "Unknown job, a sale that is part of a deal or was cancelled, the amount is out of range, or it is more than is left to pay on the job", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin, or customer payments are not turned on for the company", body = ErrorBody),
        (status = 404, description = "No such customer in the company", body = ErrorBody),
        (status = 409, description = "The company's bank account is not connected", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_payment(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<NewPayment>,
) -> ApiResult<(StatusCode, Json<CustomerPayment>)> {
    let pool = &state.pool;
    let config = CustomerPayments::of(&state);
    let head = family_of(pool, user.company_id, id).await?;
    let job = parse_project_key(&input.project_key)
        .ok_or_else(|| ApiError::BadRequest("Unknown project".into()))?;
    // The job's own customer, who must be one of the family.
    let owner: Option<i32> = match job {
        Job::Deal(deal) => {
            sqlx::query_scalar(AssertSqlSafe(format!(
                "SELECT d.customer_id FROM deals d
                  WHERE d.id = ? AND d.deleted_at IS NULL AND d.customer_id IN ({FAMILY_SQL})"
            )))
            .bind(deal)
            .bind(user.company_id)
            .bind(head)
            .bind(head)
            .fetch_optional(pool)
            .await?
        }
        Job::Sale(sale) => {
            // With the deal the sale's payments show under, if any (the
            // one `payment_job_joins` finds).
            let found: Option<(i32, bool, Option<i64>)> = sqlx::query_as(AssertSqlSafe(format!(
                "SELECT s.customer_id, s.cancelled_date IS NOT NULL,
                        (SELECT CAST(d.id AS SIGNED) FROM deals d
                          WHERE d.id = s.deal_id AND d.deleted_at IS NULL)
                   FROM sales s
                  WHERE s.id = ? AND s.customer_id IN ({FAMILY_SQL})"
            )))
            .bind(sale)
            .bind(user.company_id)
            .bind(head)
            .bind(head)
            .fetch_optional(pool)
            .await?;
            match found {
                // Its payments would show under the deal and count toward
                // the deal's balance, not the sale's.
                Some((_, _, Some(deal))) => {
                    return Err(ApiError::BadRequest(format!(
                        "This sale is part of project #{deal}: make the link for the project"
                    )));
                }
                Some((_, true, _)) => {
                    return Err(ApiError::BadRequest(
                        "This sale was cancelled, so there is nothing to pay for it".into(),
                    ));
                }
                Some((owner, ..)) => Some(owner),
                None => None,
            }
        }
    };
    let owner = owner.ok_or_else(|| ApiError::BadRequest("Unknown project".into()))?;
    if !(MIN_AMOUNT_CENTS..=MAX_AMOUNT_CENTS).contains(&input.amount_cents) {
        return Err(ApiError::BadRequest(
            "The amount must be between $1 and $999,999.99".into(),
        ));
    }
    if input.method == PaymentMethod::Card && input.amount_cents < MIN_CARD_AMOUNT_CENTS {
        return Err(ApiError::BadRequest(
            "A card payment must be at least $10. Use ACH for a smaller amount".into(),
        ));
    }
    // Links of a Stripe account the company no longer uses ask for nothing.
    withdraw_stale_links(&state, user.company_id).await?;
    // No link for more than is left to pay on the job: a typo must not make
    // the customer overpay.
    let (price_cents, taken) = job_balance(pool, user.company_id, &job).await?;
    let open_cents = (price_cents - taken.paid - taken.pending).max(0);
    let free_cents = (open_cents - taken.unpaid_links).max(0);
    if input.amount_cents > free_cents {
        return Err(ApiError::BadRequest(if taken.unpaid_links > 0 {
            format!(
                "Unpaid links already ask for {}: this one can be for up to {}",
                dollars(taken.unpaid_links),
                dollars(free_cents)
            )
        } else {
            format!("The open balance is {}", dollars(free_cents))
        }));
    }
    let settings = settings_row(pool, user.company_id).await?;
    turned_on(&settings)?;
    if settings.status(config.demo) != BankAccountStatus::Connected {
        return Err(ApiError::Conflict(
            "Connect the company's bank account first (Admin → Company → Payments)".into(),
        ));
    }
    let account = settings.account(config.demo).unwrap_or_default();
    let share_bps = settings
        .share_bps(input.method)
        .clamp(0, input.method.rate_bps());
    let split = split_fee(input.method, input.amount_cents, share_bps);
    let public_id = new_public_id()?;
    let (deal_id, sale_id) = match job {
        Job::Deal(deal) => (Some(deal), None),
        Job::Sale(sale) => (None, Some(sale)),
    };
    sqlx::query(
        "INSERT INTO customer_payments
                (public_id, company_id, customer_id, deal_id, sale_id, method, amount_cents,
                 fee_cents, customer_fee_cents, customer_fee_bps, company_fee_cents,
                 application_fee_cents, stripe_account_id, created_by, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, UTC_TIMESTAMP())",
    )
    .bind(&public_id)
    .bind(user.company_id)
    .bind(owner)
    .bind(deal_id)
    .bind(sale_id)
    .bind(input.method.as_str())
    .bind(input.amount_cents)
    .bind(split.fee)
    .bind(split.customer_fee)
    .bind(if split.customer_fee > 0 { share_bps } else { 0 })
    .bind(split.company_fee)
    .bind(split.application_fee)
    .bind(account)
    .bind(user.id)
    .execute(pool)
    .await?;
    let row = payment_by_id(pool, &public_id)
        .await?
        .ok_or_else(|| ApiError::Internal("the payment was not saved".into()))?;
    let origin = company_origin(&state, user.company_id).await?;
    Ok((StatusCode::CREATED, Json(row.into_payment(&origin))))
}

/// What the payments of one job already cover, in cents.
#[derive(sqlx::FromRow)]
struct JobTaken {
    /// Paid (or disputed and not decided yet), less what was refunded.
    paid: i64,
    /// ACH transfers on their way.
    pending: i64,
    /// Links nobody paid yet.
    unpaid_links: i64,
}

/// A job's price (a deal's amount, else its open sales' prices; a sale's
/// price) and what its payments already cover, the way the CRM's Projects
/// tab works out the open balance. A job's payments are the ones the list
/// shows under it (`payment_job_joins`): a deal's include those made for
/// its sales before they joined it. A disputed payment still covers its
/// amount until the bank decides: the company holds the money.
async fn job_balance(
    pool: &MySqlPool,
    company_id: i32,
    job: &Job,
) -> sqlx::Result<(i64, JobTaken)> {
    let (price_sql, of_job, id) = match job {
        Job::Deal(deal) => (
            "SELECT CAST(ROUND(COALESCE(d.amount,
                         (SELECT SUM(COALESCE(s.price, 0)) FROM sales s
                           WHERE s.deal_id = d.id AND s.cancelled_date IS NULL), 0) * 100)
                         AS SIGNED)
               FROM deals d WHERE d.id = ?",
            concat!(payment_deal!(), " = ?"),
            *deal,
        ),
        Job::Sale(sale) => (
            "SELECT CAST(ROUND(COALESCE(s.price, 0) * 100) AS SIGNED) FROM sales s WHERE s.id = ?",
            concat!("p.sale_id = ? AND ", payment_deal!(), " IS NULL"),
            i64::from(*sale),
        ),
    };
    let price_cents: Option<i64> = sqlx::query_scalar(price_sql)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    let taken: JobTaken = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT CAST(COALESCE(SUM(IF(p.status IN ('paid', 'disputed'),
                         p.amount_cents - LEAST(p.refunded_cents, p.amount_cents), 0)), 0)
                     AS SIGNED) AS paid,
                CAST(COALESCE(SUM(IF(p.status = 'pending', p.amount_cents, 0)), 0) AS SIGNED)
                    AS pending,
                CAST(COALESCE(SUM(IF(p.status = 'created', p.amount_cents, 0)), 0) AS SIGNED)
                    AS unpaid_links
           FROM customer_payments p
           {joins}
          WHERE p.company_id = ? AND {of_job}",
        joins = payment_job_joins!(),
    )))
    .bind(company_id)
    .bind(id)
    .fetch_one(pool)
    .await?;
    Ok((price_cents.unwrap_or(0), taken))
}

/// Withdraw a payment link
///
/// Cancels a link nobody paid yet (`created`): its page then says it is no
/// longer active, and its open Stripe page is closed. A payment that was
/// made, is on its way or failed cannot be canceled (409). Canceling a
/// canceled link again changes nothing.
#[crate::openapi::path(
    delete,
    path = "/v1/customers/{id}/payments/{payment_id}",
    operation_id = "cancel_customer_payment",
    tag = "customer-payments",
    params(
        ("id" = i32, Path, description = "Customer id"),
        ("payment_id" = String, Path, description = "The payment's id (`pay_...`)"),
    ),
    responses(
        (status = 204, description = "Canceled"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such customer or payment in the company", body = ErrorBody),
        (status = 409, description = "The payment was already made or started", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn cancel_payment(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, payment_id)): Path<(i32, String)>,
) -> ApiResult<StatusCode> {
    let pool = &state.pool;
    let head = family_of(pool, user.company_id, id).await?;
    let found: Option<(String, String)> = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT CAST(p.status AS CHAR), p.stripe_account_id
           FROM customer_payments p
          WHERE p.public_id = ? AND p.company_id = ? AND p.customer_id IN ({FAMILY_SQL})"
    )))
    .bind(&payment_id)
    .bind(user.company_id)
    .bind(user.company_id)
    .bind(head)
    .bind(head)
    .fetch_optional(pool)
    .await?;
    let (status, account) = found.ok_or(ApiError::NotFound)?;
    if status == "canceled" {
        return Ok(StatusCode::NO_CONTENT);
    }
    // Under the row's lock, so a Checkout page opened meanwhile (`POST
    // /v1/public/pay/{payment_id}/checkout`) is the one closed below.
    let mut tx = pool.begin().await?;
    let session: Option<Option<String>> = sqlx::query_scalar(
        "SELECT stripe_session_id FROM customer_payments
          WHERE public_id = ? AND status = 'created' FOR UPDATE",
    )
    .bind(&payment_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(session) = session else {
        return Err(ApiError::Conflict(
            "This payment was already made or started, so it cannot be canceled".into(),
        ));
    };
    sqlx::query("UPDATE customer_payments SET status = 'canceled' WHERE public_id = ?")
        .bind(&payment_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    // So nobody pays on a page opened before. A payment that still arrives
    // is recorded: the money is the company's either way.
    if let Some(session) = session.filter(|_| !account.starts_with(DEMO_ACCOUNT_PREFIX))
        && let Err(error) = state
            .stripe
            .expire_checkout_session(&account, &session)
            .await
    {
        tracing::warn!(%error, payment = %payment_id, "Checkout session not closed");
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---- the payment page

/// What the payment page (`/pay/<payment id>`) shows.
#[derive(Debug, Serialize, ToSchema)]
pub struct PayPage {
    /// The company being paid.
    pub company_name: String,
    /// The job: the deal's title, else the sale's address; null without
    /// either.
    pub project_name: Option<String>,
    /// How the customer pays.
    pub method: PaymentMethod,
    /// Where the payment is: only `created` can be paid.
    pub status: CustomerPaymentStatus,
    /// What goes toward the job, in US cents.
    pub amount_cents: i32,
    /// The customer's part of the fee, added on top.
    pub customer_fee_cents: i32,
    /// That part as a share of the amount, in basis points (50 = 0.5%).
    pub customer_fee_bps: i32,
    /// What the customer is charged: the two above together.
    pub total_cents: i32,
    /// Demo mode: nothing is charged, and the page reports the outcome
    /// itself with `POST /v1/public/pay/{payment_id}/demo`.
    pub demo: bool,
}

async fn page_payment(state: &AppState, payment_id: &str) -> ApiResult<PaymentRow> {
    payment_by_id(&state.pool, payment_id)
        .await?
        .ok_or(ApiError::NotFound)
}

/// Show a payment link's page
///
/// What the customer is asked to pay, to whom and for which job, and whether
/// it can still be paid (`status` is `created`). Once the customer opened
/// Stripe's page, Stripe is asked where the payment is (at most once a
/// minute), so the page is right even when Stripe's own message was missed.
/// A link made for a Stripe account the company no longer uses is withdrawn
/// (`canceled`): it would send the money to the old account.
/// No session: the link's
/// unguessable id is the credential. Next: `POST
/// /v1/public/pay/{payment_id}/checkout`.
#[crate::openapi::path(
    get,
    path = "/v1/public/pay/{payment_id}",
    operation_id = "get_pay_page",
    tag = "customer-payments",
    params(("payment_id" = String, Path, description = "The last part of the payment link (`pay_...`)")),
    responses(
        (status = 200, description = "The payment", body = PayPage),
        (status = 404, description = "No such payment link", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
)]
pub async fn pay_page(
    State(state): State<AppState>,
    Path(payment_id): Path<String>,
) -> ApiResult<Response> {
    let mut payment = page_payment(&state, &payment_id).await?;
    // Made for a Stripe account the company no longer uses: withdrawn, the
    // page then says to ask for a new link.
    if payment.of_another_account() {
        withdraw_stale_links(&state, payment.company_id).await?;
        payment = page_payment(&state, &payment_id).await?;
    }
    // The customer may be back from Stripe before its webhook, or the
    // webhook may never come: Stripe is asked, at most once a minute.
    if catch_up(&state, &payment, CHECK_FROM_PAGE_SECS).await? {
        payment = page_payment(&state, &payment_id).await?;
    }
    let company_name: Option<String> = sqlx::query_scalar(
        "SELECT COALESCE(NULLIF(TRIM(name), ''), CONCAT('Company ', id))
           FROM company WHERE id = ?",
    )
    .bind(payment.company_id)
    .fetch_optional(&state.pool)
    .await?;
    Ok(no_store(Json(PayPage {
        company_name: company_name.unwrap_or_default(),
        method: payment.method(),
        status: payment.status(),
        total_cents: payment.amount_cents + payment.customer_fee_cents,
        amount_cents: payment.amount_cents,
        customer_fee_cents: payment.customer_fee_cents,
        customer_fee_bps: payment.customer_fee_bps,
        project_name: payment.project_name,
        demo: CustomerPayments::of(&state).demo,
    })))
}

/// Where the customer pays.
#[derive(Debug, Serialize, ToSchema)]
pub struct PayCheckout {
    /// Stripe's Checkout page to send the customer to; it comes back to the
    /// payment link with `?checkout=success` or `?checkout=cancel`. Null in
    /// demo mode.
    pub url: Option<String>,
}

/// Why a payment that is not `created` cannot be paid.
fn not_payable(status: CustomerPaymentStatus) -> ApiError {
    ApiError::Conflict(
        match status {
            CustomerPaymentStatus::Canceled => "This payment link is no longer active",
            CustomerPaymentStatus::Pending => "This payment is already on its way",
            CustomerPaymentStatus::Failed => "This payment did not go through. Ask for a new link",
            _ => "This payment was already made",
        }
        .into(),
    )
}

/// Who and what a payment is for, on its Stripe session and payment intent:
/// the company, the customer, the job (`project_key`, `deal_id`, `sale_id`)
/// and its quote (the signed one, else the newest; for a sale, the quote it
/// was sold from). Empty values are left out.
async fn checkout_metadata(
    pool: &MySqlPool,
    payment: &PaymentRow,
) -> sqlx::Result<Vec<(&'static str, String)>> {
    let quote: Option<i32> = sqlx::query_scalar(
        "SELECT q.id FROM quotes q
          WHERE q.company_id = ? AND q.deleted_at IS NULL
            AND (q.id IN (SELECT so.quote_id FROM sales_orders so
                           WHERE so.sale_id = ? AND so.company_id = q.company_id)
                 OR (q.deal_id IS NOT NULL AND q.deal_id = ?))
          ORDER BY q.signed_at IS NULL, q.signed_at DESC, q.id DESC
          LIMIT 1",
    )
    .bind(payment.company_id)
    .bind(payment.sale_id)
    .bind(payment.deal_id)
    .fetch_optional(pool)
    .await?;
    let mut metadata = vec![
        ("company_id", payment.company_id.to_string()),
        ("customer_id", payment.customer_id.to_string()),
        ("project_key", payment.project_key()),
    ];
    if let Some(deal) = payment.deal_id {
        metadata.push(("deal_id", deal.to_string()));
    }
    if let Some(sale) = payment.sale_id {
        metadata.push(("sale_id", sale.to_string()));
    }
    if let Some(quote) = quote {
        metadata.push(("quote_id", quote.to_string()));
    }
    Ok(metadata)
}

/// Open Stripe's page for a payment link
///
/// A Stripe Checkout page on the company's account for the payment's amount
/// plus the customer's part of the fee, with the payment's one method. A
/// page opened before is reused while Stripe keeps it open (24 hours), and
/// requests at the same time (two tabs) get the same page, so a payment is
/// not made twice. 409 unless the payment is `created`, and for a link made
/// for a Stripe account the company no longer uses (the link is withdrawn;
/// the company makes a new one, which charges its account of today).
#[crate::openapi::path(
    post,
    path = "/v1/public/pay/{payment_id}/checkout",
    operation_id = "create_pay_checkout",
    tag = "customer-payments",
    params(("payment_id" = String, Path, description = "The last part of the payment link (`pay_...`)")),
    responses(
        (status = 200, description = "Stripe's page", body = PayCheckout),
        (status = 404, description = "No such payment link", body = ErrorBody),
        (status = 409, description = "The payment was made, is on its way, failed or was withdrawn, or the link was made for a Stripe account the company no longer uses", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Stripe failed", body = ErrorBody),
        (status = 503, description = "Payments are not set up", body = ErrorBody),
    ),
)]
pub async fn pay_checkout(
    State(state): State<AppState>,
    Path(payment_id): Path<String>,
) -> ApiResult<Response> {
    let payment = page_payment(&state, &payment_id).await?;
    if payment.status() != CustomerPaymentStatus::Created {
        return Err(not_payable(payment.status()));
    }
    // A link charges the Stripe account it was made for. When the company
    // uses another one now, the money would go to the old account: the link
    // is withdrawn, and the company makes a new one.
    if payment.of_another_account() {
        withdraw_stale_links(&state, payment.company_id).await?;
        return Err(not_payable(CustomerPaymentStatus::Canceled));
    }
    if CustomerPayments::of(&state).demo {
        return Ok(no_store(Json(PayCheckout { url: None })));
    }
    let account = payment.stripe_account_id.as_str();
    // Never without a connected account: Stripe would charge the platform's
    // own account.
    if !account.starts_with("acct_")
        || account.starts_with(DEMO_ACCOUNT_PREFIX)
        || !state.stripe.is_enabled()
    {
        return Err(ApiError::Unavailable("Payments are not set up".into()));
    }
    // Read before the row is locked: the pool has one connection, and the
    // transaction below keeps it until the Checkout page is saved.
    let link = payment.url(&company_origin(&state, payment.company_id).await?);
    let metadata = checkout_metadata(&state.pool, &payment).await?;
    // One Checkout page per payment at a time: the payment's row stays
    // locked until its page is saved, so a second tab (or a second click)
    // waits here and then reuses that page instead of opening another one
    // the customer could pay on as well.
    let mut tx = state.pool.begin().await?;
    let locked: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT CAST(status AS CHAR), stripe_session_id FROM customer_payments
          WHERE public_id = ? FOR UPDATE",
    )
    .bind(&payment.public_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (status, previous) = locked.ok_or(ApiError::NotFound)?;
    let status = CustomerPaymentStatus::parse(&status);
    if status != CustomerPaymentStatus::Created {
        return Err(not_payable(status));
    }
    if let Some(session) = &previous {
        let session = state
            .stripe
            .checkout_session(account, session)
            .await
            .map_err(stripe_error)?;
        match (session["status"].as_str(), session["url"].as_str()) {
            (Some("open"), Some(url)) => {
                return Ok(no_store(Json(PayCheckout {
                    url: Some(url.into()),
                })));
            }
            // Paid there, or a transfer was started there: the webhook is
            // about to say so.
            (Some("complete"), _) => {
                return Err(not_payable(if session["payment_status"] == "unpaid" {
                    CustomerPaymentStatus::Pending
                } else {
                    CustomerPaymentStatus::Paid
                }));
            }
            _ => {}
        }
    }
    let fee_name = fee_line_name(payment.customer_fee_bps);
    let description = payment.checkout_name();
    // The same for every request that follows the same page (none yet, or
    // one Stripe let expire) and asks for the same thing: Stripe makes one
    // session for them all. The client adds a digest of what is sent, so a
    // request that changed since a failed try is not refused for reusing
    // the key.
    // The hour is in it because Stripe also replays an answer that was its
    // own failure (a 500) for the key's 24 hours: the link is payable again
    // within the hour. A second session is harmless: one that was not saved
    // here was never shown to anyone.
    let idempotency_key = format!(
        "granite-checkout-{}-after-{}-h{}",
        payment.public_id,
        previous.as_deref().unwrap_or("none"),
        chrono::Utc::now().timestamp() / 3_600
    );
    let session = state
        .stripe
        .create_payment_checkout_session(
            account,
            &PaymentCheckout {
                payment_id: &payment.public_id,
                payment_method: payment.method().stripe_type(),
                description: &description,
                amount_cents: i64::from(payment.amount_cents),
                customer_fee_cents: i64::from(payment.customer_fee_cents),
                customer_fee_name: &fee_name,
                metadata: &metadata,
                application_fee_cents: i64::from(payment.application_fee_cents),
                success_url: &format!("{link}?checkout=success"),
                cancel_url: &format!("{link}?checkout=cancel"),
                idempotency_key: &idempotency_key,
            },
        )
        .await
        .map_err(stripe_error)?;
    let (Some(id), Some(url)) = (session["id"].as_str(), session["url"].as_str()) else {
        return Err(ApiError::Upstream(
            "Stripe returned no Checkout page".into(),
        ));
    };
    sqlx::query("UPDATE customer_payments SET stripe_session_id = ? WHERE public_id = ?")
        .bind(id)
        .bind(&payment.public_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(no_store(Json(PayCheckout {
        url: Some(url.into()),
    })))
}

/// What the demo payment page reports.
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum DemoOutcome {
    /// The money arrived.
    Paid,
    /// An ACH transfer started.
    Pending,
    /// The transfer did not go through.
    Failed,
}

/// Body of `POST /v1/public/pay/{payment_id}/demo`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DemoPayment {
    /// What happened to the payment.
    pub outcome: DemoOutcome,
}

/// Report a demo payment's outcome
///
/// Demo mode only (`CUSTOMER_PAYMENTS_DEMO=1`, local runs; 404 otherwise):
/// stands in for Stripe's webhook, moving the payment on and notifying the
/// company exactly as a real payment would. Nothing is charged.
#[crate::openapi::path(
    post,
    path = "/v1/public/pay/{payment_id}/demo",
    operation_id = "report_demo_payment",
    tag = "customer-payments",
    params(("payment_id" = String, Path, description = "The last part of the payment link (`pay_...`)")),
    request_body = DemoPayment,
    responses(
        (status = 204, description = "Recorded (or the payment was already past that point)"),
        (status = 400, description = "Unknown outcome", body = ErrorBody),
        (status = 404, description = "Not in demo mode, or no such payment link", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
)]
pub async fn pay_demo(
    State(state): State<AppState>,
    Path(payment_id): Path<String>,
    JsonBody(input): JsonBody<DemoPayment>,
) -> ApiResult<StatusCode> {
    if !CustomerPayments::of(&state).demo {
        return Err(ApiError::NotFound);
    }
    let payment = page_payment(&state, &payment_id).await?;
    let outcome = match (input.outcome, payment.method()) {
        (DemoOutcome::Paid, _) => Outcome::Paid,
        (DemoOutcome::Pending, _) => Outcome::Pending,
        // A declined card leaves the link payable, as on Stripe.
        (DemoOutcome::Failed, PaymentMethod::Card) => {
            attempt_failed(&state.pool, &payment.public_id, None, None, "card declined").await?;
            return Ok(StatusCode::NO_CONTENT);
        }
        (DemoOutcome::Failed, PaymentMethod::Ach) => Outcome::Failed("insufficient funds".into()),
    };
    settle(&state.pool, &payment.public_id, None, None, outcome).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- what Stripe reports

/// Something that happened to a payment.
enum Outcome {
    /// An ACH transfer started.
    Pending,
    /// The money arrived.
    Paid,
    /// It did not go through, and why.
    Failed(String),
    /// The customer disputed it.
    Disputed,
    /// The dispute was decided for the company: paid again.
    DisputeWon,
    /// The dispute was decided for the customer: the money went back.
    DisputeLost,
}

impl Outcome {
    /// The status it leads to, and the statuses it may follow.
    const fn step(&self) -> (&'static str, &'static str) {
        match self {
            Self::Pending => ("pending", "'created', 'canceled'"),
            Self::Paid => ("paid", "'created', 'canceled', 'pending'"),
            Self::Failed(_) => ("failed", "'created', 'canceled', 'pending'"),
            Self::Disputed => ("disputed", "'paid'"),
            Self::DisputeWon => ("paid", "'disputed'"),
            // From `paid` too: the dispute's close can come before its
            // opening, which then changes nothing.
            Self::DisputeLost => ("lost", "'paid', 'disputed'"),
        }
    }

    /// `notifications.notification_type` of its bell row.
    const fn notification(&self) -> Option<&'static str> {
        match self {
            Self::Pending => Some("payment_pending"),
            Self::Paid => Some("payment_paid"),
            Self::Failed(_) => Some("payment_failed"),
            Self::Disputed => Some("payment_disputed"),
            Self::DisputeWon => None,
            Self::DisputeLost => Some("payment_dispute_lost"),
        }
    }
}

/// Moves a payment on and, when that changed it, tells the company, all in
/// one transaction. Stripe may send an event twice or out of order: a step
/// that does not follow the payment's status changes nothing. Only an ACH
/// payment can be pending. With `account`, only a payment of that connected
/// account is touched.
async fn settle(
    pool: &MySqlPool,
    public_id: &str,
    account: Option<&str>,
    intent: Option<&str>,
    outcome: Outcome,
) -> sqlx::Result<bool> {
    let mut tx = pool.begin().await?;
    let changed = settle_in(&mut tx, public_id, account, intent, outcome).await?;
    tx.commit().await?;
    Ok(changed)
}

/// [`settle`] once per `key` (a step of a Stripe object, such as
/// `dispute_created:dp_1`), and not at all once `unless` was recorded (the
/// step that undoes it, such as the dispute's close): the same event sent
/// again, or one that arrives after the event that settled it, changes
/// nothing. `outcome` None only records `key`.
async fn settle_once(
    pool: &MySqlPool,
    key: &str,
    unless: Option<&str>,
    public_id: &str,
    account: &str,
    outcome: Option<Outcome>,
) -> sqlx::Result<bool> {
    let mut tx = pool.begin().await?;
    let first = sqlx::query("INSERT IGNORE INTO customer_payment_events (event_id) VALUES (?)")
        .bind(key)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 1;
    let undone: Option<String> = match unless {
        Some(unless) => {
            sqlx::query_scalar("SELECT event_id FROM customer_payment_events WHERE event_id = ?")
                .bind(unless)
                .fetch_optional(&mut *tx)
                .await?
        }
        None => None,
    };
    let changed = match outcome {
        Some(outcome) if first && undone.is_none() => {
            settle_in(&mut tx, public_id, Some(account), None, outcome).await?
        }
        _ => false,
    };
    tx.commit().await?;
    Ok(changed)
}

async fn settle_in(
    tx: &mut sqlx::MySqlConnection,
    public_id: &str,
    account: Option<&str>,
    intent: Option<&str>,
    outcome: Outcome,
) -> sqlx::Result<bool> {
    let (status, from) = outcome.step();
    let reason = match &outcome {
        Outcome::Failed(reason) => Some(reason.chars().take(MESSAGE_CHARS).collect::<String>()),
        _ => None,
    };
    let changed = sqlx::query(AssertSqlSafe(format!(
        "UPDATE customer_payments
            SET status = '{status}', failure_reason = ?,
                paid_at = IF('{status}' = 'paid', COALESCE(paid_at, UTC_TIMESTAMP()), paid_at),
                stripe_payment_intent_id = COALESCE(?, stripe_payment_intent_id)
          WHERE public_id = ? AND status IN ({from})
            AND ('{status}' <> 'pending' OR method = 'ach')
            AND (? IS NULL OR stripe_account_id = ?)"
    )))
    .bind(&reason)
    .bind(intent)
    .bind(public_id)
    .bind(account)
    .bind(account)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if changed && let Some(kind) = outcome.notification() {
        notify(&mut *tx, public_id, kind, Bell::default()).await?;
    } else if !changed
        && matches!(outcome, Outcome::Paid | Outcome::Pending)
        && let (Some(intent), Some(account)) = (intent, account)
    {
        paid_twice(&mut *tx, public_id, account, intent).await?;
    }
    Ok(changed)
}

/// A payment arrived on a link someone already paid (or whose transfer is
/// on its way) with another payment intent: the customer paid twice. The
/// company hears once per intent, to refund the extra one in Stripe.
async fn paid_twice(
    tx: &mut sqlx::MySqlConnection,
    public_id: &str,
    account: &str,
    intent: &str,
) -> sqlx::Result<()> {
    let other: Option<Option<String>> = sqlx::query_scalar(
        "SELECT stripe_payment_intent_id FROM customer_payments
          WHERE public_id = ? AND stripe_account_id = ?
            AND status IN ('pending', 'paid', 'refunded', 'disputed', 'lost')",
    )
    .bind(public_id)
    .bind(account)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(Some(other)) = other else {
        return Ok(());
    };
    if other == intent {
        return Ok(());
    }
    let first = sqlx::query("INSERT IGNORE INTO customer_payment_events (event_id) VALUES (?)")
        .bind(format!("paid_twice:{intent}"))
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 1;
    if first {
        tracing::error!(payment = public_id, intent, "a payment link was paid twice");
        notify(&mut *tx, public_id, "payment_duplicate", Bell::default()).await?;
    }
    Ok(())
}

/// A try to pay that failed on a link that stays payable (a declined card):
/// the link stays `created`, so the customer can try again, and the company
/// hears why. `event`: Stripe's event id, so an event sent twice is heard
/// once (none in demo mode). True when the company was told.
async fn attempt_failed(
    pool: &MySqlPool,
    public_id: &str,
    account: Option<&str>,
    event: Option<&str>,
    reason: &str,
) -> sqlx::Result<bool> {
    let mut tx = pool.begin().await?;
    if let Some(event) = event {
        let first = sqlx::query("INSERT IGNORE INTO customer_payment_events (event_id) VALUES (?)")
            .bind(event)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            == 1;
        if !first {
            return Ok(false);
        }
    }
    let payable: Option<u64> = sqlx::query_scalar(
        "SELECT id FROM customer_payments
          WHERE public_id = ? AND status = 'created' AND (? IS NULL OR stripe_account_id = ?)
          FOR UPDATE",
    )
    .bind(public_id)
    .bind(account)
    .bind(account)
    .fetch_optional(&mut *tx)
    .await?;
    if payable.is_some() {
        let bell = Bell {
            reason: Some(reason),
            ..Bell::default()
        };
        notify(&mut tx, public_id, "payment_failed", bell).await?;
    }
    tx.commit().await?;
    Ok(payable.is_some())
}

/// Money given back on a payment (`charge.refunded`). `refunded_total`: all
/// that was given back so far, in cents of what the customer was charged
/// (Stripe's `amount_refunded`); `full`: the whole charge was. A partial
/// refund leaves the payment `paid`; the whole one makes it `refunded`. The
/// company hears how much each refund gave back. The total only grows, so an
/// event sent twice or late changes nothing.
async fn refund(
    pool: &MySqlPool,
    public_id: &str,
    account: &str,
    refunded_total: Option<i64>,
    full: bool,
) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    let row: Option<(String, i64, i64)> = sqlx::query_as(
        "SELECT CAST(status AS CHAR), CAST(refunded_cents AS SIGNED),
                CAST(amount_cents + customer_fee_cents AS SIGNED)
           FROM customer_payments
          WHERE public_id = ? AND stripe_account_id = ?
          FOR UPDATE",
    )
    .bind(public_id)
    .bind(account)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((status, before, charged)) = row else {
        return Ok(());
    };
    if !matches!(status.as_str(), "paid" | "disputed" | "refunded") {
        return Ok(());
    }
    let total = refunded_total
        .unwrap_or(if full { charged } else { before })
        .clamp(0, charged)
        .max(before);
    let given_back = total - before;
    let now_refunded = full && status != "refunded";
    if given_back == 0 && !now_refunded {
        return Ok(());
    }
    sqlx::query(
        "UPDATE customer_payments
            SET refunded_cents = ?, status = IF(?, 'refunded', status)
          WHERE public_id = ?",
    )
    .bind(total)
    .bind(now_refunded)
    .bind(public_id)
    .execute(&mut *tx)
    .await?;
    let bell = Bell {
        amount_cents: Some(if given_back > 0 { given_back } else { charged }),
        ..Bell::default()
    };
    notify(&mut tx, public_id, "payment_refunded", bell).await?;
    tx.commit().await
}

/// The payment intent of a charge that is not the payment's own: the link
/// was paid twice (`paid_twice`) and `intent` is the second charge's. None
/// when it is the payment's own charge, or the payment has none kept yet.
async fn extra_charge(
    pool: &MySqlPool,
    public_id: &str,
    account: &str,
    intent: Option<&str>,
) -> sqlx::Result<Option<String>> {
    let Some(intent) = intent else {
        return Ok(None);
    };
    let own: Option<Option<String>> = sqlx::query_scalar(
        "SELECT stripe_payment_intent_id FROM customer_payments
          WHERE public_id = ? AND stripe_account_id = ?",
    )
    .bind(public_id)
    .bind(account)
    .fetch_optional(pool)
    .await?;
    Ok(match own.flatten() {
        Some(own) if own != intent => Some(intent.to_string()),
        _ => None,
    })
}

/// Money given back on the second charge of a link paid twice: the payment
/// itself stays as it is, and the company hears that the extra charge went
/// back, once per amount given back so far.
async fn extra_refunded(
    pool: &MySqlPool,
    public_id: &str,
    account: &str,
    intent: &str,
    refunded_total: Option<i64>,
) -> sqlx::Result<()> {
    let total = refunded_total.unwrap_or(0);
    let mut tx = pool.begin().await?;
    let first = sqlx::query("INSERT IGNORE INTO customer_payment_events (event_id) VALUES (?)")
        .bind(format!("extra_refunded:{intent}:{total}"))
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 1;
    let ours: Option<u64> = sqlx::query_scalar(
        "SELECT id FROM customer_payments WHERE public_id = ? AND stripe_account_id = ?",
    )
    .bind(public_id)
    .bind(account)
    .fetch_optional(&mut *tx)
    .await?;
    if first && ours.is_some() && total > 0 {
        let bell = Bell {
            amount_cents: Some(total),
            ..Bell::default()
        };
        notify(&mut tx, public_id, "payment_duplicate_refunded", bell).await?;
    }
    tx.commit().await
}

/// What a bell row says beyond the payment itself.
#[derive(Default, Clone, Copy)]
struct Bell<'a> {
    /// The amount to name instead of the payment's (a refund's).
    amount_cents: Option<i64>,
    /// Why it failed, instead of the payment's `failure_reason`.
    reason: Option<&'a str>,
}

/// What the bell says about a payment.
fn message(payment: &PaymentRow, kind: &str, bell: Bell<'_>) -> String {
    let customer = payment.customer_name.as_deref();
    let project = payment
        .project_name
        .clone()
        .unwrap_or_else(|| payment.project_number().to_lowercase());
    let amount = dollars(
        bell.amount_cents
            .unwrap_or_else(|| i64::from(payment.amount_cents)),
    );
    let method = payment.method().words();
    let text = match kind {
        "payment_pending" => format!(
            "{} started an ACH payment of {amount} for {project}. It takes about 4 business days.",
            customer.unwrap_or("A customer")
        ),
        "payment_failed" => format!(
            "{}'s {amount} {method} payment for {project} failed: {}.",
            customer.unwrap_or("A customer"),
            bell.reason
                .or(payment.failure_reason.as_deref())
                .unwrap_or("the payment was declined")
                .trim_end_matches('.')
        ),
        "payment_refunded" => format!(
            "{amount} was refunded to {} for {project}.",
            customer.unwrap_or("the customer")
        ),
        "payment_disputed" => format!(
            "{} disputed the {amount} payment for {project}.",
            customer.unwrap_or("A customer")
        ),
        "payment_dispute_lost" => format!(
            "The dispute of the {amount} payment for {project} was lost: the bank gave the money back to {}.",
            customer.unwrap_or("the customer")
        ),
        "payment_duplicate_refunded" => format!(
            "The extra {amount} payment for {project} was refunded to {}. The first payment stays.",
            customer.unwrap_or("the customer")
        ),
        "payment_duplicate" => format!(
            "{} paid {amount} by {method} for {project} a second time. Refund the extra payment in Stripe.",
            customer.unwrap_or("A customer")
        ),
        _ => format!(
            "{} paid {amount} by {method} for {project}.",
            customer.unwrap_or("A customer")
        ),
    };
    text.chars().take(MESSAGE_CHARS).collect()
}

/// A bell row for each Manager of the company and for whoever made the link;
/// it opens the payment's project.
async fn notify(
    conn: &mut sqlx::MySqlConnection,
    public_id: &str,
    kind: &str,
    bell: Bell<'_>,
) -> sqlx::Result<()> {
    let Some(payment) = payment_by_id(&mut *conn, public_id).await? else {
        return Ok(());
    };
    let users: Vec<i32> = sqlx::query_scalar(
        "SELECT u.id FROM users u
           JOIN users_positions up ON up.user_id = u.id
          WHERE u.is_deleted = 0 AND up.company_id = ? AND up.position_id = ?
          UNION
         SELECT u.id FROM users u WHERE u.is_deleted = 0 AND u.id = ?
          ORDER BY id",
    )
    .bind(payment.company_id)
    .bind(MANAGER_POSITION)
    .bind(payment.created_by)
    .fetch_all(&mut *conn)
    .await?;
    let message = message(&payment, kind, bell);
    for user in users {
        sqlx::query(
            "INSERT INTO notifications
                    (user_id, customer_id, customer_payment_id, message, notification_type, due_at)
             VALUES (?, ?, ?, ?, ?, NOW())",
        )
        .bind(user)
        .bind(payment.customer_id)
        .bind(payment.id)
        .bind(&message)
        .bind(kind)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Our id of the payment a Stripe object (a Checkout session, a payment
/// intent, a charge) was made for.
fn payment_of(object: &Value) -> Option<&str> {
    object["metadata"]["granite_payment"]
        .as_str()
        .or_else(|| object["client_reference_id"].as_str())
        .filter(|id| !id.is_empty())
}

/// An id field: a string, or an expanded object's `id`.
fn id_of(value: &Value) -> Option<&str> {
    value.as_str().or_else(|| value["id"].as_str())
}

/// The payment a charge or dispute belongs to: by the charge's metadata,
/// else by its payment intent. Stripe keeps no order: a refund or a dispute
/// can come before the event that says the payment was made, when the
/// intent is not kept with the payment yet. Then the intent's own metadata
/// says which payment it is, read from Stripe.
async fn payment_of_charge(
    state: &AppState,
    account: &str,
    object: &Value,
) -> sqlx::Result<Option<String>> {
    if let Some(id) = payment_of(object) {
        return Ok(Some(id.to_string()));
    }
    let Some(intent) = id_of(&object["payment_intent"]) else {
        return Ok(None);
    };
    let kept: Option<String> = sqlx::query_scalar(
        "SELECT public_id FROM customer_payments
          WHERE stripe_payment_intent_id = ? AND stripe_account_id = ?",
    )
    .bind(intent)
    .bind(account)
    .fetch_optional(&state.pool)
    .await?;
    if kept.is_some() || !state.stripe.is_enabled() {
        return Ok(kept);
    }
    match state.stripe.payment_intent(account, intent).await {
        Ok(found) => Ok(payment_of(&found).map(str::to_string)),
        Err(error) => {
            tracing::warn!(%error, intent, "payment intent of a charge not read");
            Ok(None)
        }
    }
}

/// Why an ACH transfer failed, in plain words (see `decline_reason`).
async fn failure_reason(state: &AppState, account: &str, intent: Option<&str>) -> String {
    let Some(intent) = intent.filter(|_| state.stripe.is_enabled()) else {
        return decline_reason(PaymentMethod::Ach, &Value::Null);
    };
    match state.stripe.payment_intent(account, intent).await {
        Ok(intent) => decline_reason(PaymentMethod::Ach, &intent["last_payment_error"]),
        Err(error) => {
            tracing::warn!(%error, "payment intent not read");
            decline_reason(PaymentMethod::Ach, &Value::Null)
        }
    }
}

/// Why a payment failed, from a payment intent's `last_payment_error`, as a
/// short phrase a manager reads in the bell and on the payment: the decline
/// or return code (with or without Stripe's `payment_method_` prefix), else
/// key words of Stripe's message. Unknown reasons read "card declined" or
/// "the bank declined the transfer"; a lost or stolen card reads as declined.
fn decline_reason(method: PaymentMethod, error: &Value) -> String {
    let text = |key: &str| error[key].as_str().unwrap_or_default().to_ascii_lowercase();
    let codes = [text("decline_code"), text("code")];
    let has = |names: &[&str]| {
        codes.iter().any(|code| {
            let code = code.strip_prefix("payment_method_").unwrap_or(code);
            names.contains(&code)
        })
    };
    let message = text("message");
    let says = |words: &[&str]| words.iter().any(|word| message.contains(word));
    // Stripe's own message goes to people only through these short phrases:
    // it is English written for developers ("Your request used a real card
    // while testing...").
    let words = if has(&["insufficient_funds"]) || says(&["insufficient funds"]) {
        "insufficient funds"
    } else if method == PaymentMethod::Ach {
        // A bounced transfer often carries "generic_decline"; the return
        // code or Stripe's message says why.
        if has(&["account_closed"]) || says(&["closed"]) {
            "the bank account is closed"
        } else if has(&["no_account", "invalid_account_number"])
            || says(&["could not be located", "invalid account"])
        {
            "the bank account was not found"
        } else if has(&["debit_not_authorized", "debit_authorization_not_match"])
            || says(&["not authorized", "unauthorized"])
        {
            "the customer's bank refused the debit"
        } else if has(&["account_frozen", "bank_account_restricted"])
            || says(&["frozen", "restricted"])
        {
            "the bank account is frozen or restricted"
        } else {
            "the bank declined the transfer"
        }
    } else if has(&["expired_card"]) {
        "the card has expired"
    } else if has(&["incorrect_cvc", "invalid_cvc"]) {
        "wrong security code"
    } else if has(&["incorrect_number", "invalid_number"]) {
        "wrong card number"
    } else if has(&["invalid_expiry_month", "invalid_expiry_year"]) {
        "wrong expiry date"
    } else if has(&["card_velocity_exceeded", "withdrawal_count_limit_exceeded"]) {
        "the card's limit was reached"
    } else if has(&[
        "authentication_required",
        "payment_intent_authentication_failure",
    ]) {
        "the bank's confirmation (3D Secure) was not completed"
    } else if has(&["card_not_supported", "currency_not_supported"]) {
        "this card is not accepted"
    } else if has(&["processing_error"]) {
        "the card could not be charged, try again"
    } else if has(&["test_mode_live_card", "live_mode_test_card"]) {
        "a real card was used in test mode"
    } else {
        "card declined"
    };
    words.into()
}

/// A failed payment intent: a declined card (or a bank that refused at once)
/// leaves the link payable and only tells the company; an ACH transfer on
/// its way that bounced fails the payment.
async fn intent_failed(
    pool: &MySqlPool,
    account: &str,
    event: Option<&str>,
    object: &Value,
) -> sqlx::Result<()> {
    let Some(payment) = payment_of(object) else {
        return Ok(());
    };
    let found: Option<(String, String)> = sqlx::query_as(
        "SELECT CAST(status AS CHAR), CAST(method AS CHAR) FROM customer_payments
          WHERE public_id = ? AND stripe_account_id = ?",
    )
    .bind(payment)
    .bind(account)
    .fetch_optional(pool)
    .await?;
    let Some((status, method)) = found else {
        return Ok(());
    };
    let reason = decline_reason(PaymentMethod::parse(&method), &object["last_payment_error"]);
    match status.as_str() {
        "created" => {
            attempt_failed(pool, payment, Some(account), event, &reason).await?;
        }
        "pending" => {
            settle(
                pool,
                payment,
                Some(account),
                object["id"].as_str(),
                Outcome::Failed(reason),
            )
            .await?;
        }
        _ => {}
    }
    Ok(())
}

/// A canceled payment intent. Only a transfer we hold to be on its way
/// fails by it. A link nobody paid stays payable: Stripe may cancel the
/// intent of a Checkout page that was left open, and the link outlives its
/// pages.
async fn intent_canceled(pool: &MySqlPool, account: &str, object: &Value) -> sqlx::Result<()> {
    let Some(payment) = payment_of(object) else {
        return Ok(());
    };
    let found: Option<(String, String)> = sqlx::query_as(
        "SELECT CAST(status AS CHAR), CAST(method AS CHAR) FROM customer_payments
          WHERE public_id = ? AND stripe_account_id = ?",
    )
    .bind(payment)
    .bind(account)
    .fetch_optional(pool)
    .await?;
    let Some((status, method)) = found else {
        return Ok(());
    };
    if status == "pending" {
        let reason = transfer_failure(PaymentMethod::parse(&method), object);
        settle(
            pool,
            payment,
            Some(account),
            object["id"].as_str(),
            Outcome::Failed(reason),
        )
        .await?;
    }
    Ok(())
}

/// What a Checkout page that finished unpaid (`checkout.session.completed`
/// with `payment_status: unpaid`) means for its payment. The session does
/// not say whether the transfer started or the customer has yet to confirm
/// a bank account typed by hand, so its payment intent is read from Stripe
/// (`transfer_outcome`). When Stripe does not answer, nothing is recorded
/// here: `payment_intent.processing` and `catch_up` say it later. Without a
/// Stripe client or an intent to ask about, the transfer is taken as started.
async fn unpaid_session_outcome(
    state: &AppState,
    account: &str,
    payment: &str,
    intent: Option<&str>,
) -> sqlx::Result<Option<Outcome>> {
    let Some(intent) = intent.filter(|_| state.stripe.is_enabled()) else {
        return Ok(Some(Outcome::Pending));
    };
    let method: Option<String> = sqlx::query_scalar(
        "SELECT CAST(method AS CHAR) FROM customer_payments
          WHERE public_id = ? AND stripe_account_id = ?",
    )
    .bind(payment)
    .bind(account)
    .fetch_optional(&state.pool)
    .await?;
    let Some(method) = method else {
        return Ok(None);
    };
    match state.stripe.payment_intent(account, intent).await {
        Ok(found) => Ok(transfer_outcome(PaymentMethod::parse(&method), &found)),
        Err(error) => {
            tracing::warn!(%error, payment, "payment intent of an unpaid session not read");
            Ok(None)
        }
    }
}

/// Applies one event of a connected account.
async fn apply_event(state: &AppState, event: &Value) -> sqlx::Result<()> {
    let pool = &state.pool;
    let object = &event["data"]["object"];
    let kind = event["type"].as_str().unwrap_or_default();
    if kind == "account.updated" {
        return save_account(pool, object).await;
    }
    // The company cut the platform off from its Stripe account: nothing can
    // be charged on it any more, so it no longer counts as connected and no
    // new link is made for it.
    if kind == "account.application.deauthorized" {
        if let Some(account) = event["account"].as_str() {
            tracing::error!(account, "a company disconnected its Stripe account");
            sqlx::query(
                "UPDATE company_payment_settings SET charges_enabled = 0
                  WHERE stripe_account_id = ?",
            )
            .bind(account)
            .execute(pool)
            .await?;
        }
        return Ok(());
    }
    // Only events of a connected account are about customers' payments.
    let Some(account) = event["account"].as_str() else {
        return Ok(());
    };
    let intent = id_of(&object["payment_intent"]);
    match kind {
        "checkout.session.completed" | "checkout.session.async_payment_succeeded" => {
            let Some(payment) = payment_of(object) else {
                return Ok(());
            };
            let outcome = match object["payment_status"].as_str() {
                Some("paid") => Outcome::Paid,
                // ACH: the page is finished and the money is not there yet.
                Some("unpaid") if kind == "checkout.session.completed" => {
                    match unpaid_session_outcome(state, account, payment, intent).await? {
                        Some(outcome) => outcome,
                        None => return Ok(()),
                    }
                }
                _ => return Ok(()),
            };
            settle(pool, payment, Some(account), intent, outcome).await?;
        }
        "checkout.session.async_payment_failed" => {
            let Some(payment) = payment_of(object) else {
                return Ok(());
            };
            let reason = failure_reason(state, account, intent).await;
            settle(
                pool,
                payment,
                Some(account),
                intent,
                Outcome::Failed(reason),
            )
            .await?;
        }
        "payment_intent.succeeded" => {
            if let Some(payment) = payment_of(object) {
                let id = object["id"].as_str();
                settle(pool, payment, Some(account), id, Outcome::Paid).await?;
            }
        }
        // ACH: the transfer started (a card's is never pending).
        "payment_intent.processing" => {
            if let Some(payment) = payment_of(object) {
                let id = object["id"].as_str();
                settle(pool, payment, Some(account), id, Outcome::Pending).await?;
            }
        }
        "payment_intent.payment_failed" => {
            intent_failed(pool, account, event["id"].as_str(), object).await?;
        }
        "payment_intent.canceled" => {
            intent_canceled(pool, account, object).await?;
        }
        "charge.refunded" => {
            if let Some(payment) = payment_of_charge(state, account, object).await? {
                let full = object["refunded"].as_bool() == Some(true);
                let total = object["amount_refunded"].as_i64();
                if let Some(extra) = extra_charge(pool, &payment, account, intent).await? {
                    extra_refunded(pool, &payment, account, &extra, total).await?;
                } else {
                    // Only a payment that was made can be refunded: when
                    // this came before the event that says so, the payment
                    // is marked paid here, or the refund would be dropped.
                    // Not for a charge still on its way (a bank transfer
                    // that has not cleared): that money has not arrived.
                    if object["status"] == "succeeded" {
                        settle(pool, &payment, Some(account), intent, Outcome::Paid).await?;
                    }
                    refund(pool, &payment, account, total, full).await?;
                }
            }
        }
        // Every dispute event (opened, updated, funds withdrawn, closed):
        // the dispute's own status says where it is.
        _ if kind.starts_with("charge.dispute.") => {
            dispute_event(state, account, object).await?;
        }
        _ => {}
    }
    Ok(())
}

/// Something happened to a dispute of a charge (`object` is the dispute).
/// Its own `status` says where it is, whichever event carries it: decided
/// for the customer (`lost`: a bank transfer's dispute is final and can
/// arrive already lost), decided for the company (`won`, or an inquiry that
/// was dropped, `warning_closed`), else still open. Once per dispute, and
/// never opened after it was decided: Stripe sending the dispute again
/// after it was won must not dispute the payment anew.
async fn dispute_event(state: &AppState, account: &str, object: &Value) -> sqlx::Result<()> {
    let pool = &state.pool;
    let intent = id_of(&object["payment_intent"]);
    let (Some(payment), Some(dispute)) = (
        payment_of_charge(state, account, object).await?,
        object["id"].as_str(),
    ) else {
        return Ok(());
    };
    // A dispute of the second charge of a link paid twice is not about the
    // payment itself: Stripe tells the company.
    if extra_charge(pool, &payment, account, intent)
        .await?
        .is_some()
    {
        tracing::warn!(payment = %payment, dispute, "dispute of an extra charge");
        return Ok(());
    }
    // Only a payment that was made can be disputed: when the dispute came
    // first, the payment is marked paid here, and the event that says so
    // changes nothing later.
    settle(pool, &payment, Some(account), intent, Outcome::Paid).await?;
    let closed = format!("dispute_closed:{dispute}");
    // Won, or an inquiry the bank dropped: the company keeps the money.
    // Lost: the bank took it back.
    let decided = match object["status"].as_str() {
        Some("won" | "warning_closed") => Some(Outcome::DisputeWon),
        Some("lost") => Some(Outcome::DisputeLost),
        _ => None,
    };
    if decided.is_some() {
        settle_once(pool, &closed, None, &payment, account, decided).await?;
        return Ok(());
    }
    settle_once(
        pool,
        &format!("dispute_created:{dispute}"),
        Some(&closed),
        &payment,
        account,
        Some(Outcome::Disputed),
    )
    .await?;
    Ok(())
}

/// Answer to Stripe.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConnectWebhookReceived {
    /// Always true.
    pub received: bool,
}

/// Receive a Stripe Connect event
///
/// Stripe's webhook for the companies' connected accounts: Checkout,
/// payment intent, refund and dispute events move a customer's payment on
/// (and notify the company), and account events update the company's bank
/// status. A declined card only notifies: the link stays payable. A partial
/// refund keeps the payment `paid` and records what was given back. A
/// dispute makes it `disputed`, then `paid` again when won or `lost` when
/// the bank gives the money back (the dispute's own status decides, on any
/// `charge.dispute.*` event). `account.application.deauthorized` (the
/// company cut the platform off from its Stripe account) leaves the company
/// not connected. Events of an account only move payments made for that
/// account, also after the company changed to another one. Each
/// change is made once, however often Stripe sends the event. Needs a valid
/// `Stripe-Signature` for `STRIPE_CONNECT_WEBHOOK_SECRET`; no session.
#[crate::openapi::path(
    post,
    path = "/v1/webhooks/stripe-connect",
    operation_id = "receive_stripe_connect_webhook",
    tag = "customer-payments",
    request_body(content = Object, description = "The Stripe event, as Stripe sent it"),
    params(("Stripe-Signature" = String, Header, description = "Stripe's signature of the body")),
    responses(
        (status = 200, description = "Processed (or ignored)", body = ConnectWebhookReceived),
        (status = 400, description = "Missing or invalid signature, or not configured", body = ErrorBody),
        (status = 500, description = "Processing failed; Stripe retries", body = ErrorBody),
    )
)]
pub async fn stripe_connect_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<ConnectWebhookReceived>> {
    let config = CustomerPayments::of(&state);
    let signature = headers
        .get("stripe-signature")
        .and_then(|value| value.to_str().ok());
    let (Some(signature), Some(secret)) = (signature, config.webhook_secret.as_deref()) else {
        return Err(ApiError::BadRequest("Webhook is not configured".into()));
    };
    if let Err(error) = verify_signature(&body, signature, secret, chrono::Utc::now().timestamp()) {
        tracing::warn!(%error, "Stripe Connect webhook signature refused");
        return Err(ApiError::BadRequest("Invalid webhook signature".into()));
    }
    let event: Value = serde_json::from_slice(&body)
        .map_err(|_| ApiError::BadRequest("Invalid webhook payload".into()))?;
    if let Err(error) = apply_event(&state, &event).await {
        tracing::error!(%error, "Stripe Connect webhook processing failed");
        return Err(ApiError::Internal("Webhook processing failed".into()));
    }
    Ok(Json(ConnectWebhookReceived { received: true }))
}

// ---- the customer's own page

/// A payment as the customer sees it: the company's side of the fee is left
/// out.
#[derive(Debug, Serialize, ToSchema)]
pub struct ViewPayment {
    /// The payment's id (`pay_...`).
    pub id: String,
    /// The job it is for: `deal-<deal id>` or `sale-<sale id>`, one of
    /// `projects`.
    pub project_key: String,
    /// The job's name: the deal's title, else the sale's address; null
    /// without either.
    pub project_name: Option<String>,
    /// The sale it was made for, when it was made for a sale.
    pub sale_id: Option<i32>,
    /// How it is paid.
    pub method: PaymentMethod,
    /// Where it is; withdrawn links are not listed.
    pub status: CustomerPaymentStatus,
    /// What goes toward the job, in US cents.
    pub amount_cents: i32,
    /// The customer's part of the fee, added on top.
    pub customer_fee_cents: i32,
    /// What was given back to the customer, in US cents (0 unless refunded).
    pub refunded_cents: i32,
    /// The page to pay on, while `created`.
    pub url: String,
    /// Why it failed, as a short plain phrase ("insufficient funds"); null
    /// unless `failed`.
    pub failure_reason: Option<String>,
    /// When the link was made (ISO 8601, UTC).
    pub created_at: String,
}

/// One job of the customer: a deal they signed, or a sale that is not part
/// of such a deal. The same jobs the CRM lists on the customer's Projects
/// tab (`GET /v1/customers/{id}/projects`).
#[derive(Debug, Serialize, ToSchema, PartialEq, Eq)]
pub struct ViewJob {
    /// `deal-<deal id>` or `sale-<sale id>`.
    pub project_key: String,
    /// The deal's title, else the address of the work; null without either.
    pub name: Option<String>,
    /// What it costs, in US cents: the deal's amount, else its sales' prices.
    pub price_cents: i64,
}

/// What the customer paid and still owes.
#[derive(Debug, Serialize, ToSchema)]
pub struct CustomerViewPayments {
    /// Customer payments are turned on for the company. While false the
    /// customer's page shows payments only when there are some (links made
    /// before it was turned off): no balance for a company that takes its
    /// money another way.
    pub enabled: bool,
    /// What their jobs cost together (`projects`), in US cents.
    pub total_cents: i64,
    /// What they paid through payment links, less what was refunded. A
    /// disputed payment counts until the bank decides; a lost one does not.
    pub paid_cents: i64,
    /// ACH transfers on their way.
    pub pending_cents: i64,
    /// `total_cents` minus `paid_cents`, never below zero. Money paid
    /// another way (a check, cash) is not counted.
    pub balance_cents: i64,
    /// Their jobs, oldest first: each deal that is won or has a sale, then
    /// each sale that is not part of such a deal. Cancelled sales are left
    /// out.
    pub projects: Vec<ViewJob>,
    /// Their payments and open payment links, newest first.
    pub payments: Vec<ViewPayment>,
}

#[derive(sqlx::FromRow)]
struct JobDeal {
    id: i64,
    title: Option<String>,
    amount_cents: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct JobSale {
    id: i32,
    deal_id: Option<i64>,
    price_cents: i64,
    address: Option<String>,
    cancelled: bool,
}

/// The customer's jobs, as `projects_of` lists them for the CRM: each deal
/// with the sales made for it, then the open sales of no listed deal.
fn jobs_of(deals: &[JobDeal], sales: &[JobSale]) -> Vec<ViewJob> {
    let mut jobs: Vec<ViewJob> = deals
        .iter()
        .map(|deal| {
            let own = || sales.iter().filter(|sale| sale.deal_id == Some(deal.id));
            ViewJob {
                project_key: format!("deal-{}", deal.id),
                name: deal
                    .title
                    .clone()
                    .or_else(|| own().find_map(|sale| sale.address.clone())),
                price_cents: deal.amount_cents.unwrap_or_else(|| {
                    own()
                        .filter(|sale| !sale.cancelled)
                        .map(|sale| sale.price_cents)
                        .sum()
                }),
            }
        })
        .collect();
    for sale in sales.iter().filter(|sale| !sale.cancelled) {
        let listed = sale
            .deal_id
            .is_some_and(|id| deals.iter().any(|deal| deal.id == id));
        if !listed {
            jobs.push(ViewJob {
                project_key: format!("sale-{}", sale.id),
                name: sale.address.clone(),
                price_cents: sale.price_cents,
            });
        }
    }
    jobs
}

/// List the customer's payments
///
/// The customer's jobs (the deals they signed and their sales) with what
/// they cost, what the customer paid through payment links and what is
/// left, with each payment and each link still to pay. Needs a verified
/// visitor.
#[crate::openapi::path(
    get,
    path = "/v1/public/customer-view/{company_id}/{view_id}/payments",
    operation_id = "list_customer_view_payments",
    tag = "customer-view",
    params(
        ("company_id" = i32, Path, description = "Company id"),
        ("view_id" = String, Path, description = "The customer's page id (a UUID)"),
    ),
    responses(
        (status = 200, description = "The jobs, the payments and the balance", body = CustomerViewPayments),
        (status = 401, description = "No code entered for this page", body = ErrorBody),
        (status = 404, description = "No such page", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn view_payments(
    State(state): State<AppState>,
    Path((company_id, view_id)): Path<(i32, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let customer = verified_customer(&state, &headers, company_id, &view_id).await?;
    let deals: Vec<JobDeal> = sqlx::query_as(
        "SELECT CAST(d.id AS SIGNED) AS id, NULLIF(TRIM(d.title), '') AS title,
                CAST(ROUND(d.amount * 100) AS SIGNED) AS amount_cents
           FROM deals d
          WHERE d.customer_id = ? AND d.deleted_at IS NULL
            AND (d.is_won = 1 OR EXISTS (SELECT 1 FROM sales s WHERE s.deal_id = d.id))
          ORDER BY d.created_at, d.id",
    )
    .bind(customer.id)
    .fetch_all(&state.pool)
    .await?;
    let sales: Vec<JobSale> = sqlx::query_as(
        "SELECT s.id, CAST(s.deal_id AS SIGNED) AS deal_id,
                CAST(ROUND(COALESCE(s.price, 0) * 100) AS SIGNED) AS price_cents,
                NULLIF(TRIM(s.project_address), '') AS address,
                s.cancelled_date IS NOT NULL AS cancelled
           FROM sales s WHERE s.customer_id = ? ORDER BY s.sale_date, s.id",
    )
    .bind(customer.id)
    .fetch_all(&state.pool)
    .await?;
    let projects = jobs_of(&deals, &sales);
    let total_cents: i64 = projects.iter().map(|job| job.price_cents).sum();
    let listed = || async {
        sqlx::query_as::<_, PaymentRow>(AssertSqlSafe(format!(
            "{PAYMENT_SELECT}
              WHERE p.company_id = ? AND p.customer_id = ? AND p.status <> 'canceled'
              ORDER BY p.created_at DESC, p.id DESC"
        )))
        .bind(company_id)
        .bind(customer.id)
        .fetch_all(&state.pool)
        .await
    };
    let mut rows = listed().await?;
    // No "Pay now" on a link of a Stripe account the company no longer uses.
    if rows.iter().any(PaymentRow::of_another_account) {
        withdraw_stale_links(&state, company_id).await?;
        rows = listed().await?;
    }
    let paid_cents: i64 = rows.iter().map(PaymentRow::paid_toward_job).sum();
    let pending_cents: i64 = rows
        .iter()
        .filter(|row| row.status() == CustomerPaymentStatus::Pending)
        .map(|row| i64::from(row.amount_cents))
        .sum();
    let origin = company_origin(&state, company_id).await?;
    let payments = rows
        .into_iter()
        .map(|row| ViewPayment {
            project_key: row.project_key(),
            method: row.method(),
            status: row.status(),
            url: row.url(&origin),
            created_at: row.created_at(),
            id: row.public_id,
            project_name: row.project_name,
            sale_id: row.sale_id,
            amount_cents: row.amount_cents,
            customer_fee_cents: row.customer_fee_cents,
            refunded_cents: row.refunded_cents,
            failure_reason: row.failure_reason,
        })
        .collect();
    let enabled = settings_row(&state.pool, company_id).await?.enabled;
    Ok(no_store(Json(CustomerViewPayments {
        enabled,
        total_cents,
        paid_cents,
        pending_cents,
        balance_cents: (total_cents - paid_cents).max(0),
        projects,
        payments,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fee_is_split_like_the_crm_previews_it() {
        // $1,000 by card, the company pays all: 3% = $30; Stripe takes about
        // $29.30 of it, so $0.70 is left for the platform.
        assert_eq!(
            split_fee(PaymentMethod::Card, 100_000, 0),
            FeeSplit {
                fee: 3_000,
                customer_fee: 0,
                company_fee: 3_000,
                application_fee: 70,
            }
        );
        // $1,000 by ACH, the customer pays 0.4% of the 1%: Stripe's 0.8% of
        // $1,004 is capped at $5.
        assert_eq!(
            split_fee(PaymentMethod::Ach, 100_000, 40),
            FeeSplit {
                fee: 1_000,
                customer_fee: 400,
                company_fee: 600,
                application_fee: 500,
            }
        );
        // Half cents round up, and the parts still add up.
        let split = split_fee(PaymentMethod::Ach, 150, 50);
        assert_eq!(
            (split.fee, split.customer_fee, split.company_fee),
            (2, 1, 1)
        );
    }

    #[test]
    fn the_platform_never_pays_to_take_a_payment() {
        // $100 by card: 3% would be $3.00 and Stripe takes $3.20, so the
        // fee is the $9 minimum and $5.80 is left.
        let small = split_fee(PaymentMethod::Card, 10_000, 0);
        assert_eq!(
            (small.fee, small.company_fee, small.application_fee),
            (900, 900, 580)
        );
        // $300 is where 3% reaches the minimum.
        assert_eq!(split_fee(PaymentMethod::Card, 30_000, 0).fee, 900);
        assert_eq!(split_fee(PaymentMethod::Card, 30_100, 0).fee, 903);
        // The smallest card payment still holds its fee.
        let least = split_fee(PaymentMethod::Card, 1_000, 0);
        assert_eq!((least.fee, least.application_fee), (900, 841));
        // ACH has no minimum.
        assert_eq!(split_fee(PaymentMethod::Ach, 10_000, 0).fee, 100);
        // A share above the rate is the whole fee, never more.
        let split = split_fee(PaymentMethod::Ach, 100_000, 900);
        assert_eq!((split.customer_fee, split.company_fee), (1_000, 0));
    }

    #[test]
    fn money_reads_like_the_crm_writes_it() {
        assert_eq!(dollars(500), "$5");
        assert_eq!(dollars(750), "$7.50");
        assert_eq!(dollars(100_000), "$1,000");
        assert_eq!(dollars(123_456_705), "$1,234,567.05");
        assert_eq!(dollars(99), "$0.99");
    }

    #[test]
    fn the_fee_line_names_the_customer_s_share() {
        assert_eq!(fee_line_name(50), "Processing fee (0.5%)");
        assert_eq!(fee_line_name(100), "Processing fee (1%)");
        assert_eq!(fee_line_name(5), "Processing fee (0.05%)");
        assert_eq!(fee_line_name(0), "Processing fee");
    }

    #[test]
    fn a_decline_reads_in_plain_words() {
        let card = |error: Value| decline_reason(PaymentMethod::Card, &error);
        assert_eq!(
            card(
                serde_json::json!({ "code": "card_declined", "decline_code": "insufficient_funds",
                                     "message": "Your card has insufficient funds." })
            ),
            "insufficient funds"
        );
        assert_eq!(
            card(serde_json::json!({ "code": "card_declined", "decline_code": "stolen_card" })),
            "card declined"
        );
        assert_eq!(
            card(serde_json::json!({ "code": "processing_error",
                                     "message": "An error occurred while processing your card." })),
            "the card could not be charged, try again"
        );
        // Stripe's developer wording never reaches the bell.
        assert_eq!(
            card(serde_json::json!({ "code": "card_declined",
                "decline_code": "test_mode_live_card",
                "message": "Your card was declined. Your request used a real card while testing." })),
            "a real card was used in test mode"
        );
        assert_eq!(
            card(
                serde_json::json!({ "code": "card_declined", "decline_code": "something_new",
                                     "message": "Something Stripe added later." })
            ),
            "card declined"
        );
        assert_eq!(card(Value::Null), "card declined");
        assert_eq!(
            decline_reason(PaymentMethod::Ach, &Value::Null),
            "the bank declined the transfer"
        );
        // What Stripe sends when a bank transfer bounces.
        assert_eq!(
            decline_reason(
                PaymentMethod::Ach,
                &serde_json::json!({ "code": "no_account", "decline_code": "generic_decline",
                    "message": "The customer's bank account could not be located." })
            ),
            "the bank account was not found"
        );
        let ach = |error: Value| decline_reason(PaymentMethod::Ach, &error);
        // What dev's test bank sent on 2026-11-20.
        assert_eq!(
            ach(serde_json::json!({ "decline_code": "generic_decline",
                "message": "The customer's bank account has been closed." })),
            "the bank account is closed"
        );
        assert_eq!(
            ach(
                serde_json::json!({ "code": "payment_method_insufficient_funds",
                "message": "The customer's account has insufficient funds to cover this payment." })
            ),
            "insufficient funds"
        );
        assert_eq!(
            ach(serde_json::json!({ "code": "debit_not_authorized", "message": "" })),
            "the customer's bank refused the debit"
        );
        assert_eq!(
            ach(serde_json::json!({ "message": "Something the bank said." })),
            "the bank declined the transfer"
        );
    }

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

    #[test]
    fn project_keys_name_a_deal_or_a_sale() {
        assert!(matches!(parse_project_key("deal-12"), Some(Job::Deal(12))));
        assert!(matches!(parse_project_key("sale-3"), Some(Job::Sale(3))));
        for bad in [
            "", "deal", "deal-", "deal--1", "sale-3-4", "quote-1", "sale-x", "deal-+1",
        ] {
            assert!(parse_project_key(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn the_jobs_of_a_customer_are_their_deals_and_the_sales_outside_them() {
        let deal = |id, title: Option<&str>, amount_cents| JobDeal {
            id,
            title: title.map(str::to_string),
            amount_cents,
        };
        let sale = |id, deal_id, price_cents, address: Option<&str>, cancelled| JobSale {
            id,
            deal_id,
            price_cents,
            address: address.map(str::to_string),
            cancelled,
        };
        let job = |key: &str, name: Option<&str>, price_cents| ViewJob {
            project_key: key.into(),
            name: name.map(str::to_string),
            price_cents,
        };
        let deals = [deal(1, Some("Kitchen"), Some(500_000)), deal(2, None, None)];
        let sales = [
            // Part of a deal with its own amount: the deal's amount counts.
            sale(10, Some(1), 480_000, Some("1 Main St"), false),
            // A deal without an amount costs its open sales, and is named
            // by their address.
            sale(11, Some(2), 100_000, Some("2 Elm St"), false),
            sale(12, Some(2), 70_000, None, true),
            // On their own; a deal that is not listed does not hide a sale.
            sale(13, None, 30_000, None, false),
            sale(14, Some(9), 20_000, Some("9 Oak Ave"), false),
            sale(15, None, 99_000, Some("gone"), true),
        ];
        assert_eq!(
            jobs_of(&deals, &sales),
            vec![
                job("deal-1", Some("Kitchen"), 500_000),
                job("deal-2", Some("2 Elm St"), 100_000),
                job("sale-13", None, 30_000),
                job("sale-14", Some("9 Oak Ave"), 20_000),
            ]
        );
        assert_eq!(jobs_of(&[], &[]), vec![]);
    }

    #[test]
    fn a_demo_account_counts_only_in_demo_mode() {
        let row = SettingsRow {
            stripe_account_id: Some("acct_demo_100".into()),
            charges_enabled: true,
            ..SettingsRow::default()
        };
        assert_eq!(row.status(true), BankAccountStatus::Connected);
        assert_eq!(row.status(false), BankAccountStatus::NotConnected);
        let real = SettingsRow {
            stripe_account_id: Some("acct_1".into()),
            ..SettingsRow::default()
        };
        assert_eq!(real.status(false), BankAccountStatus::PendingVerification);
        assert_eq!(
            SettingsRow::default().status(true),
            BankAccountStatus::NotConnected
        );
    }
}
