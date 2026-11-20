//! The payment links of a customer's jobs: listing, making and withdrawing them.

use super::catch_up::{catch_up, withdraw_stale_links};
use super::fees::{PaymentMethod, dollars, split_fee};
use super::payment::{
    CustomerPayment, PAYMENT_SELECT, PaymentRow, payment_by_id, payment_deal, payment_job_joins,
};
use super::settings::{BankAccountStatus, company_origin, settings_row, turned_on};
use super::{
    CHECK_FROM_LIST_MOST, CHECK_FROM_LIST_SECS, CustomerPayments, DEMO_ACCOUNT_PREFIX,
    MAX_AMOUNT_CENTS, MIN_AMOUNT_CENTS, MIN_CARD_AMOUNT_CENTS,
};
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use lambda_http::tracing;
use serde::Deserialize;
use sqlx::{AssertSqlSafe, MySqlPool};

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
