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

    /// `STRIPE_CONNECT_WEBHOOK_SECRET` and `CUSTOMER_PAYMENTS_DEMO=1`.
    pub fn from_env() -> Self {
        Self::new(
            &std::env::var("STRIPE_CONNECT_WEBHOOK_SECRET").unwrap_or_default(),
            std::env::var("CUSTOMER_PAYMENTS_DEMO").is_ok_and(|value| value.trim() == "1"),
        )
    }

    /// The settings `state` was given (`AppState::with_extension`), else the
    /// environment's.
    pub fn of(state: &AppState) -> Arc<Self> {
        state.extensions.get_or_insert_with(Self::from_env)
    }
}

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
    /// The bank's name; null until connected, or when Stripe does not say.
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
}

#[derive(Default, sqlx::FromRow)]
struct SettingsRow {
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
        "SELECT stripe_account_id, charges_enabled, bank_name, bank_last4,
                ach_customer_share_bps
           FROM company_payment_settings WHERE company_id = ?",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.unwrap_or_default())
}

/// Saves what Stripe says about a connected account (an `account` object).
async fn save_account(pool: &MySqlPool, account: &Value) -> sqlx::Result<()> {
    let Some(id) = account["id"].as_str() else {
        return Ok(());
    };
    let bank = &account["external_accounts"]["data"][0];
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
/// asked again on each call. Connect an account with `POST
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
        .filter(|_| !config.demo && !row.charges_enabled && state.stripe.is_enabled())
        .map(str::to_string);
    if let Some(account) = unverified {
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
        (status = 403, description = "Caller is not a superuser or an admin Manager", body = ErrorBody),
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
        (status = 403, description = "Caller is not a superuser or an admin Manager", body = ErrorBody),
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
    let row = settings_row(&state.pool, company_id).await?;
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
    /// The customer disputed the payment with their bank.
    Disputed,
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
            _ => Self::Created,
        }
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
    /// The part of the fee taken from what the company receives.
    pub company_fee_cents: i32,
    /// The page the customer pays on. It never expires.
    pub url: String,
    /// Why it failed (Stripe's words); null unless `failed`.
    pub failure_reason: Option<String>,
    /// When the link was made (ISO 8601, UTC).
    pub created_at: String,
}

#[derive(sqlx::FromRow)]
struct PaymentRow {
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
    company_fee_cents: i32,
    application_fee_cents: i32,
    stripe_account_id: String,
    stripe_session_id: Option<String>,
    failure_reason: Option<String>,
    created_by: Option<i32>,
    created_at: NaiveDateTime,
    customer_name: Option<String>,
    project_name: Option<String>,
}

/// Every payment column, with the job it shows under: a payment made for a
/// sale moves to the sale's deal once the sale is part of one, as the job
/// itself does in `GET /v1/customers/{id}/projects`.
const PAYMENT_SELECT: &str = "SELECT p.public_id, p.company_id, p.customer_id,
        CAST(COALESCE(p.deal_id, pd.id) AS SIGNED) AS deal_id, p.sale_id,
        CAST(p.method AS CHAR) AS method, CAST(p.status AS CHAR) AS status,
        p.amount_cents, p.fee_cents, p.customer_fee_cents, p.company_fee_cents,
        p.application_fee_cents, p.stripe_account_id, p.stripe_session_id,
        p.failure_reason, p.created_by, p.created_at,
        NULLIF(TRIM(pc.name), '') AS customer_name,
        COALESCE(NULLIF(TRIM(pd.title), ''), NULLIF(TRIM(ps.project_address), ''))
          AS project_name
   FROM customer_payments p
   LEFT JOIN customers pc ON pc.id = p.customer_id
   LEFT JOIN sales ps ON ps.id = p.sale_id
   LEFT JOIN deals pd ON pd.id = COALESCE(p.deal_id, ps.deal_id) AND pd.deleted_at IS NULL";

impl PaymentRow {
    fn method(&self) -> PaymentMethod {
        PaymentMethod::parse(&self.method)
    }

    fn status(&self) -> CustomerPaymentStatus {
        CustomerPaymentStatus::parse(&self.status)
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
            company_fee_cents: self.company_fee_cents,
            failure_reason: self.failure_reason,
        }
    }
}

async fn payment_by_id(pool: &MySqlPool, public_id: &str) -> sqlx::Result<Option<PaymentRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "{PAYMENT_SELECT} WHERE p.public_id = ?"
    )))
    .bind(public_id)
    .fetch_optional(pool)
    .await
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
/// `paid`; an ACH transfer on its way is `pending`.
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
    let rows: Vec<PaymentRow> = sqlx::query_as(AssertSqlSafe(format!(
        "{PAYMENT_SELECT}
          WHERE p.company_id = ? AND p.customer_id IN ({FAMILY_SQL})
          ORDER BY p.created_at DESC, p.id DESC"
    )))
    .bind(user.company_id)
    .bind(user.company_id)
    .bind(head)
    .bind(head)
    .fetch_all(&state.pool)
    .await?;
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
/// pay. 409 until the company's bank account is connected (`GET
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
        (status = 400, description = "Unknown job, or the amount is out of range", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
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
            sqlx::query_scalar(AssertSqlSafe(format!(
                "SELECT s.customer_id FROM sales s
                  WHERE s.id = ? AND s.customer_id IN ({FAMILY_SQL})"
            )))
            .bind(sale)
            .bind(user.company_id)
            .bind(head)
            .bind(head)
            .fetch_optional(pool)
            .await?
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
    let settings = settings_row(pool, user.company_id).await?;
    if settings.status(config.demo) != BankAccountStatus::Connected {
        return Err(ApiError::Conflict(
            "Connect the company's bank account first (Admin → Company → Payments)".into(),
        ));
    }
    let account = settings.account(config.demo).unwrap_or_default();
    let split = split_fee(
        input.method,
        input.amount_cents,
        settings.share_bps(input.method),
    );
    let public_id = new_public_id()?;
    let (deal_id, sale_id) = match job {
        Job::Deal(deal) => (Some(deal), None),
        Job::Sale(sale) => (None, Some(sale)),
    };
    sqlx::query(
        "INSERT INTO customer_payments
                (public_id, company_id, customer_id, deal_id, sale_id, method, amount_cents,
                 fee_cents, customer_fee_cents, company_fee_cents, application_fee_cents,
                 stripe_account_id, created_by, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, UTC_TIMESTAMP())",
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
    let found: Option<(String, Option<String>, String)> = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT CAST(p.status AS CHAR), p.stripe_session_id, p.stripe_account_id
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
    let (status, session, account) = found.ok_or(ApiError::NotFound)?;
    if status == "canceled" {
        return Ok(StatusCode::NO_CONTENT);
    }
    let canceled = sqlx::query(
        "UPDATE customer_payments SET status = 'canceled'
          WHERE public_id = ? AND status = 'created'",
    )
    .bind(&payment_id)
    .execute(pool)
    .await?
    .rows_affected()
        == 1;
    if !canceled {
        return Err(ApiError::Conflict(
            "This payment was already made or started, so it cannot be canceled".into(),
        ));
    }
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
/// it can still be paid (`status` is `created`). No session: the link's
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
    let payment = page_payment(&state, &payment_id).await?;
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

/// Open Stripe's page for a payment link
///
/// A Stripe Checkout page on the company's account for the payment's amount
/// plus the customer's part of the fee, with the payment's one method. A
/// page opened before is reused while Stripe keeps it open (24 hours), so a
/// payment is not made twice. 409 unless the payment is `created`.
#[crate::openapi::path(
    post,
    path = "/v1/public/pay/{payment_id}/checkout",
    operation_id = "create_pay_checkout",
    tag = "customer-payments",
    params(("payment_id" = String, Path, description = "The last part of the payment link (`pay_...`)")),
    responses(
        (status = 200, description = "Stripe's page", body = PayCheckout),
        (status = 404, description = "No such payment link", body = ErrorBody),
        (status = 409, description = "The payment was made, is on its way, failed or was withdrawn", body = ErrorBody),
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
    if CustomerPayments::of(&state).demo {
        return Ok(no_store(Json(PayCheckout { url: None })));
    }
    let account = payment.stripe_account_id.as_str();
    if account.starts_with(DEMO_ACCOUNT_PREFIX) || !state.stripe.is_enabled() {
        return Err(ApiError::Unavailable("Payments are not set up".into()));
    }
    if let Some(session) = &payment.stripe_session_id {
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
            // Paid there; the webhook is about to say so.
            (Some("complete"), _) => return Err(not_payable(CustomerPaymentStatus::Paid)),
            _ => {}
        }
    }
    let link = payment.url(&company_origin(&state, payment.company_id).await?);
    let description = payment
        .project_name
        .clone()
        .unwrap_or_else(|| "Your project".into());
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
                application_fee_cents: i64::from(payment.application_fee_cents),
                success_url: &format!("{link}?checkout=success"),
                cancel_url: &format!("{link}?checkout=cancel"),
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
        .execute(&state.pool)
        .await?;
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
    let outcome = match input.outcome {
        DemoOutcome::Paid => Outcome::Paid,
        DemoOutcome::Pending => Outcome::Pending,
        DemoOutcome::Failed => Outcome::Failed(match payment.method() {
            PaymentMethod::Card => "card declined".into(),
            PaymentMethod::Ach => "insufficient funds".into(),
        }),
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
    /// The whole payment was given back.
    Refunded,
    /// The customer disputed it.
    Disputed,
    /// The dispute was decided for the company: paid again.
    DisputeWon,
}

impl Outcome {
    /// The status it leads to, and the statuses it may follow.
    const fn step(&self) -> (&'static str, &'static str) {
        match self {
            Self::Pending => ("pending", "'created', 'canceled'"),
            Self::Paid => ("paid", "'created', 'canceled', 'pending'"),
            Self::Failed(_) => ("failed", "'created', 'canceled', 'pending'"),
            Self::Refunded => ("refunded", "'paid', 'disputed'"),
            Self::Disputed => ("disputed", "'paid'"),
            Self::DisputeWon => ("paid", "'disputed'"),
        }
    }

    /// `notifications.notification_type` of its bell row.
    const fn notification(&self) -> Option<&'static str> {
        match self {
            Self::Pending => Some("payment_pending"),
            Self::Paid => Some("payment_paid"),
            Self::Failed(_) => Some("payment_failed"),
            Self::Refunded => Some("payment_refunded"),
            Self::Disputed => Some("payment_disputed"),
            Self::DisputeWon => None,
        }
    }
}

/// Moves a payment on and, when that changed it, tells the company. Stripe
/// may send an event twice or out of order: a step that does not follow the
/// payment's status changes nothing. With `account`, only a payment of that
/// connected account is touched.
async fn settle(
    pool: &MySqlPool,
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
            AND (? IS NULL OR stripe_account_id = ?)"
    )))
    .bind(&reason)
    .bind(intent)
    .bind(public_id)
    .bind(account)
    .bind(account)
    .execute(pool)
    .await?
    .rows_affected()
        == 1;
    if changed && let Some(kind) = outcome.notification() {
        notify(pool, public_id, kind).await?;
    }
    Ok(changed)
}

/// What the bell says about a payment.
fn message(payment: &PaymentRow, kind: &str) -> String {
    let customer = payment.customer_name.as_deref().unwrap_or("A customer");
    let project = payment.project_name.as_deref().unwrap_or("their project");
    let amount = dollars(i64::from(payment.amount_cents));
    let method = payment.method().words();
    let text = match kind {
        "payment_pending" => format!(
            "{customer} started an ACH payment of {amount} for {project}. It takes about 4 business days."
        ),
        "payment_failed" => format!(
            "{customer}'s {amount} {method} payment for {project} failed: {}.",
            payment
                .failure_reason
                .as_deref()
                .unwrap_or("the payment was declined")
                .trim_end_matches('.')
        ),
        "payment_refunded" => {
            format!("{customer}'s {amount} {method} payment for {project} was refunded.")
        }
        "payment_disputed" => {
            format!("{customer} disputed their {amount} {method} payment for {project}.")
        }
        _ => format!("{customer} paid {amount} by {method} for {project}."),
    };
    text.chars().take(MESSAGE_CHARS).collect()
}

/// A bell row for each Manager of the company and for whoever made the link.
async fn notify(pool: &MySqlPool, public_id: &str, kind: &str) -> sqlx::Result<()> {
    let Some(payment) = payment_by_id(pool, public_id).await? else {
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
    .fetch_all(pool)
    .await?;
    let message = message(&payment, kind);
    for user in users {
        sqlx::query(
            "INSERT INTO notifications (user_id, customer_id, message, notification_type, due_at)
             VALUES (?, ?, ?, ?, NOW())",
        )
        .bind(user)
        .bind(payment.customer_id)
        .bind(&message)
        .bind(kind)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Our id of the payment a Stripe object (a Checkout session, a charge) was
/// made for.
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
/// else by its payment intent.
async fn payment_of_charge(
    pool: &MySqlPool,
    account: &str,
    object: &Value,
) -> sqlx::Result<Option<String>> {
    if let Some(id) = payment_of(object) {
        return Ok(Some(id.to_string()));
    }
    let Some(intent) = id_of(&object["payment_intent"]) else {
        return Ok(None);
    };
    sqlx::query_scalar(
        "SELECT public_id FROM customer_payments
          WHERE stripe_payment_intent_id = ? AND stripe_account_id = ?",
    )
    .bind(intent)
    .bind(account)
    .fetch_optional(pool)
    .await
}

/// Why an ACH transfer failed, in Stripe's words.
async fn failure_reason(state: &AppState, account: &str, intent: Option<&str>) -> String {
    let fallback = || "the bank declined the transfer".to_string();
    let Some(intent) = intent.filter(|_| state.stripe.is_enabled()) else {
        return fallback();
    };
    match state.stripe.payment_intent(account, intent).await {
        Ok(intent) => intent["last_payment_error"]["message"]
            .as_str()
            .map(str::trim)
            .filter(|message| !message.is_empty())
            .map_or_else(fallback, str::to_string),
        Err(error) => {
            tracing::warn!(%error, "payment intent not read");
            fallback()
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
                // ACH: the transfer has only started.
                Some("unpaid") if kind == "checkout.session.completed" => Outcome::Pending,
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
        "charge.refunded" if object["refunded"].as_bool() == Some(true) => {
            if let Some(payment) = payment_of_charge(pool, account, object).await? {
                settle(pool, &payment, Some(account), None, Outcome::Refunded).await?;
            }
        }
        "charge.dispute.created" => {
            if let Some(payment) = payment_of_charge(pool, account, object).await? {
                settle(pool, &payment, Some(account), None, Outcome::Disputed).await?;
            }
        }
        "charge.dispute.closed" if object["status"] == "won" => {
            if let Some(payment) = payment_of_charge(pool, account, object).await? {
                settle(pool, &payment, Some(account), None, Outcome::DisputeWon).await?;
            }
        }
        _ => {}
    }
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
/// refund and dispute events move a customer's payment on (and notify the
/// company), and account events update the company's bank status. Each
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
    /// The job it is for: `deal-<deal id>` or `sale-<sale id>`.
    pub project_key: String,
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
    /// The page to pay on, while `created`.
    pub url: String,
    /// Why it failed; null unless `failed`.
    pub failure_reason: Option<String>,
    /// When the link was made (ISO 8601, UTC).
    pub created_at: String,
}

/// What the customer paid and still owes.
#[derive(Debug, Serialize, ToSchema)]
pub struct CustomerViewPayments {
    /// What their jobs cost: the prices of their sales that were not
    /// cancelled, in US cents.
    pub total_cents: i64,
    /// What they paid through payment links.
    pub paid_cents: i64,
    /// ACH transfers on their way.
    pub pending_cents: i64,
    /// `total_cents` minus `paid_cents`, never below zero. Money paid
    /// another way (a check, cash) is not counted.
    pub balance_cents: i64,
    /// Their payments and open payment links, newest first.
    pub payments: Vec<ViewPayment>,
}

/// List the customer's payments
///
/// What the customer's jobs cost, what they paid through payment links and
/// what is left, with each payment and each link still to pay. Needs a
/// verified visitor.
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
        (status = 200, description = "The payments and the balance", body = CustomerViewPayments),
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
    let total_cents: i64 = sqlx::query_scalar(
        "SELECT CAST(COALESCE(SUM(ROUND(s.price * 100)), 0) AS SIGNED)
           FROM sales s WHERE s.customer_id = ? AND s.cancelled_date IS NULL",
    )
    .bind(customer.id)
    .fetch_one(&state.pool)
    .await?;
    let rows: Vec<PaymentRow> = sqlx::query_as(AssertSqlSafe(format!(
        "{PAYMENT_SELECT}
          WHERE p.company_id = ? AND p.customer_id = ? AND p.status <> 'canceled'
          ORDER BY p.created_at DESC, p.id DESC"
    )))
    .bind(company_id)
    .bind(customer.id)
    .fetch_all(&state.pool)
    .await?;
    let sum = |status: CustomerPaymentStatus| -> i64 {
        rows.iter()
            .filter(|row| row.status() == status)
            .map(|row| i64::from(row.amount_cents))
            .sum()
    };
    let paid_cents = sum(CustomerPaymentStatus::Paid);
    let pending_cents = sum(CustomerPaymentStatus::Pending);
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
            sale_id: row.sale_id,
            amount_cents: row.amount_cents,
            customer_fee_cents: row.customer_fee_cents,
            failure_reason: row.failure_reason,
        })
        .collect();
    Ok(no_store(Json(CustomerViewPayments {
        total_cents,
        paid_cents,
        pending_cents,
        balance_cents: (total_cents - paid_cents).max(0),
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
