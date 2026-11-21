//! Asking Stripe about a payment whose webhook may not have come, and
//! withdrawing the links of a Stripe account the company no longer uses.

use super::fees::PaymentMethod;
use super::payment::PaymentRow;
use super::settle::{Outcome, settle};
use super::webhook::{decline_reason, id_of};
use super::{CustomerPayments, DEMO_ACCOUNT_PREFIX};
use crate::state::AppState;
use lambda_http::tracing;
use serde_json::Value;
use sqlx::AssertSqlSafe;

/// Asks Stripe where a waiting payment is, for when its webhook did not
/// come (the server was down, the endpoint misconfigured): the payment's
/// Checkout session, and for a bank transfer its payment intent. What Stripe
/// says moves the payment on exactly as the webhook would, bells included; a
/// webhook that comes later changes nothing. At most once per `every_secs`
/// for one payment, whoever asks. True when the payment changed. Stripe not
/// answering is only logged: the caller shows what is kept.
pub(super) async fn catch_up(
    state: &AppState,
    payment: &PaymentRow,
    every_secs: i64,
) -> sqlx::Result<bool> {
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
pub(super) fn transfer_outcome(method: PaymentMethod, intent: &Value) -> Option<Outcome> {
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
pub(super) fn transfer_failure(method: PaymentMethod, intent: &Value) -> String {
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
pub(super) async fn withdraw_stale_links(state: &AppState, company_id: i32) -> sqlx::Result<u64> {
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
