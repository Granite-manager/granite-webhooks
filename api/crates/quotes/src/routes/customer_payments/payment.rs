//! A payment as the database keeps it and as the CRM is shown it.

use super::DEMO_ACCOUNT_PREFIX;
use super::fees::PaymentMethod;
use crate::openapi::ToSchema;
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use sqlx::AssertSqlSafe;

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
    pub(super) fn parse(value: &str) -> Self {
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
pub(super) struct PaymentRow {
    pub(super) id: u64,
    pub(super) public_id: String,
    pub(super) company_id: i32,
    pub(super) customer_id: i32,
    pub(super) deal_id: Option<i64>,
    pub(super) sale_id: Option<i32>,
    method: String,
    status: String,
    pub(super) amount_cents: i32,
    fee_cents: i32,
    pub(super) customer_fee_cents: i32,
    pub(super) customer_fee_bps: i32,
    company_fee_cents: i32,
    pub(super) application_fee_cents: i32,
    pub(super) refunded_cents: i32,
    pub(super) stripe_account_id: String,
    pub(super) stripe_session_id: Option<String>,
    stripe_checked_at: Option<NaiveDateTime>,
    pub(super) failure_reason: Option<String>,
    pub(super) created_by: Option<i32>,
    created_at: NaiveDateTime,
    pub(super) customer_name: Option<String>,
    pub(super) project_name: Option<String>,
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
pub(super) use payment_job_joins;

/// The deal a payment shows under (see `payment_job_joins`); NULL for a
/// payment of a sale that is not part of a deal.
macro_rules! payment_deal {
    () => {
        "COALESCE(p.deal_id, pd.id)"
    };
}
pub(super) use payment_deal;

/// Every payment column, with the job it shows under.
pub(super) const PAYMENT_SELECT: &str = concat!(
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
    pub(super) fn method(&self) -> PaymentMethod {
        PaymentMethod::parse(&self.method)
    }

    pub(super) fn status(&self) -> CustomerPaymentStatus {
        CustomerPaymentStatus::parse(&self.status)
    }

    /// The job's number as people say it: "Project #10226" (a deal) or
    /// "Sale #812" (a sale outside any deal).
    pub(super) fn project_number(&self) -> String {
        match (self.deal_id, self.sale_id) {
            (Some(deal), _) => format!("Project #{deal}"),
            (None, sale) => format!("Sale #{}", sale.unwrap_or_default()),
        }
    }

    /// What the customer sees on Stripe's page and in their statement: the
    /// job's name with its number ("12 Oak St · Project #10226"), or the
    /// number alone when the job has no name.
    pub(super) fn checkout_name(&self) -> String {
        let number = self.project_number();
        self.project_name
            .as_ref()
            .map_or_else(|| number.clone(), |name| format!("{name} · {number}"))
    }

    pub(super) fn project_key(&self) -> String {
        match (self.deal_id, self.sale_id) {
            (Some(deal), _) => format!("deal-{deal}"),
            (None, sale) => format!("sale-{}", sale.unwrap_or_default()),
        }
    }

    pub(super) fn url(&self, origin: &str) -> String {
        format!("{origin}/pay/{}", self.public_id)
    }

    pub(super) fn created_at(&self) -> String {
        self.created_at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
    }

    /// What it put toward the job: its amount while the company holds the
    /// money (paid, or disputed and not decided yet), less what was given
    /// back (a refund takes the customer's fee back last).
    pub(super) fn paid_toward_job(&self) -> i64 {
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
    pub(super) fn of_another_account(&self) -> bool {
        self.status() == CustomerPaymentStatus::Created
            && self.company_account.as_deref().unwrap_or_default() != self.stripe_account_id
    }

    /// Whether Stripe may know more than we do: the payment waits (`created`
    /// or `pending`), the customer opened Stripe's page for it, and Stripe
    /// was not asked in the last `every_secs` seconds.
    pub(super) fn may_be_behind(&self, every_secs: i64) -> bool {
        matches!(
            self.status(),
            CustomerPaymentStatus::Created | CustomerPaymentStatus::Pending
        ) && self.stripe_session_id.is_some()
            && !self.stripe_account_id.starts_with(DEMO_ACCOUNT_PREFIX)
            && self
                .stripe_checked_at
                .is_none_or(|at| (chrono::Utc::now().naive_utc() - at).num_seconds() >= every_secs)
    }

    pub(super) fn into_payment(self, origin: &str) -> CustomerPayment {
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

pub(super) async fn payment_by_id<'e>(
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
