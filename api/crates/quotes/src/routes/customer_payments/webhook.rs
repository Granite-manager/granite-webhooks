//! Stripe's Connect webhook: its events and what each does to a payment.

use super::CustomerPayments;
use super::catch_up::{transfer_failure, transfer_outcome};
use super::fees::PaymentMethod;
use super::settings::save_account;
use super::settle::{
    Outcome, attempt_failed, extra_charge, extra_refunded, refund, settle, settle_once,
};
use crate::billing::stripe::verify_signature;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use lambda_http::tracing;
use serde::Serialize;
use serde_json::Value;
use sqlx::MySqlPool;

/// Our id of the payment a Stripe object (a Checkout session, a payment
/// intent, a charge) was made for.
fn payment_of(object: &Value) -> Option<&str> {
    object["metadata"]["granite_payment"]
        .as_str()
        .or_else(|| object["client_reference_id"].as_str())
        .filter(|id| !id.is_empty())
}

/// An id field: a string, or an expanded object's `id`.
pub(super) fn id_of(value: &Value) -> Option<&str> {
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
pub(super) fn decline_reason(method: PaymentMethod, error: &Value) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
