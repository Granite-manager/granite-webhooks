//! The payment page: what it shows, Stripe's Checkout page for the payment,
//! and the outcome in demo mode.

use super::catch_up::{catch_up, withdraw_stale_links};
use super::fees::{PaymentMethod, fee_line_name};
use super::payment::{CustomerPaymentStatus, PaymentRow, payment_by_id};
use super::settings::company_origin;
use super::settle::{Outcome, attempt_failed, settle};
use super::{CHECK_FROM_PAGE_SECS, CustomerPayments, DEMO_ACCOUNT_PREFIX, stripe_error};
use crate::billing::stripe::PaymentCheckout;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::routes::customer_view::no_store;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;

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
