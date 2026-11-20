//! The company's settings: its bank account at Stripe and who pays each fee.

use super::fees::PaymentMethod;
use super::{
    ACH_RATE_BPS, CARD_RATE_BPS, CHECK_ACCOUNT_SECS, CustomerPayments, DEMO_ACCOUNT_PREFIX,
    SHARE_STEP_BPS, stripe_error,
};
use crate::app_origin::{app_origin, company_subdomain};
use crate::auth::{EmployeeUser, UserManager};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::MySqlPool;

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
pub(super) struct SettingsRow {
    pub(super) enabled: bool,
    stripe_account_id: Option<String>,
    charges_enabled: bool,
    bank_name: Option<String>,
    bank_last4: Option<String>,
    ach_customer_share_bps: i32,
}

impl SettingsRow {
    /// The company's connected account. A demo account counts only in demo
    /// mode, so one left in a database never reaches Stripe.
    pub(super) fn account(&self, demo: bool) -> Option<&str> {
        self.stripe_account_id
            .as_deref()
            .filter(|account| !account.is_empty())
            .filter(|account| demo || !account.starts_with(DEMO_ACCOUNT_PREFIX))
    }

    pub(super) fn status(&self, demo: bool) -> BankAccountStatus {
        match self.account(demo) {
            None => BankAccountStatus::NotConnected,
            Some(_) if self.charges_enabled => BankAccountStatus::Connected,
            Some(_) => BankAccountStatus::PendingVerification,
        }
    }

    pub(super) const fn share_bps(&self, method: PaymentMethod) -> i32 {
        match method {
            // Locked: see `PaymentFeeSetting::locked`.
            PaymentMethod::Card => 0,
            PaymentMethod::Ach => self.ach_customer_share_bps,
        }
    }
}

pub(super) async fn settings_row(pool: &MySqlPool, company_id: i32) -> sqlx::Result<SettingsRow> {
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
pub(super) fn turned_on(settings: &SettingsRow) -> ApiResult<()> {
    if settings.enabled {
        Ok(())
    } else {
        Err(ApiError::Denied(
            "Customer payments are not turned on for this company".into(),
        ))
    }
}

/// Saves what Stripe says about a connected account (an `account` object).
pub(super) async fn save_account(pool: &MySqlPool, account: &Value) -> sqlx::Result<()> {
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
pub(super) async fn company_origin(state: &AppState, company_id: i32) -> sqlx::Result<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
