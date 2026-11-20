//! Writing down what Stripe reports about a payment: paid, failed, refunded, paid twice.

use super::MESSAGE_CHARS;
use super::bells::{Bell, notify};
use lambda_http::tracing;
use sqlx::{AssertSqlSafe, MySqlPool};

/// Something that happened to a payment.
pub(super) enum Outcome {
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
pub(super) async fn settle(
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
pub(super) async fn settle_once(
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
pub(super) async fn attempt_failed(
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
pub(super) async fn refund(
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
pub(super) async fn extra_charge(
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
pub(super) async fn extra_refunded(
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
