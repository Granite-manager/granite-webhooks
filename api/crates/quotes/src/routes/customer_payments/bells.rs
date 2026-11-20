//! The bell: who hears about a payment's change, and in what words.

use super::fees::dollars;
use super::payment::{PaymentRow, payment_by_id};
use super::{MANAGER_POSITION, MESSAGE_CHARS};

/// What a bell row says beyond the payment itself.
#[derive(Default, Clone, Copy)]
pub(super) struct Bell<'a> {
    /// The amount to name instead of the payment's (a refund's).
    pub(super) amount_cents: Option<i64>,
    /// Why it failed, instead of the payment's `failure_reason`.
    pub(super) reason: Option<&'a str>,
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
pub(super) async fn notify(
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
