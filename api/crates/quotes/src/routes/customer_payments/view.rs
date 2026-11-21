//! The customer's own page: their jobs and what they paid.

use super::catch_up::withdraw_stale_links;
use super::fees::PaymentMethod;
use super::payment::{CustomerPaymentStatus, PAYMENT_SELECT, PaymentRow};
use super::settings::{company_origin, settings_row};
use crate::error::{ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::routes::customer_view::{no_store, verified_customer};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use serde::Serialize;
use sqlx::AssertSqlSafe;

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
}
