//! The event's history log (`event_history`). Port of
//! `app/lib/eventHistory.server.ts`.

use lambda_http::tracing;
use sqlx::MySqlPool;

/// One history line.
pub struct History<'a> {
    event_id: i32,
    company_id: i32,
    action: &'static str,
    created_by: Option<&'a str>,
    message: Option<&'a str>,
    body: Option<&'a str>,
    from_value: Option<&'a str>,
    to_value: Option<&'a str>,
    created_by_id: Option<i32>,
}

impl<'a> History<'a> {
    pub const fn new(
        event_id: i32,
        company_id: i32,
        action: &'static str,
        created_by: Option<&'a str>,
    ) -> Self {
        Self {
            event_id,
            company_id,
            action,
            created_by,
            message: None,
            body: None,
            from_value: None,
            to_value: None,
            created_by_id: None,
        }
    }

    /// What the line changed: the value before and after (either may be
    /// empty: a field set for the first time, or cleared).
    #[must_use]
    pub const fn change(mut self, from: Option<&'a str>, to: Option<&'a str>) -> Self {
        self.from_value = from;
        self.to_value = to;
        self
    }

    /// The user who made it, for their picture.
    #[must_use]
    pub const fn by_user(mut self, user_id: i32) -> Self {
        self.created_by_id = Some(user_id);
        self
    }

    /// Instead of the action's default text.
    #[must_use]
    pub const fn message(mut self, message: &'a str) -> Self {
        self.message = Some(message);
        self
    }

    /// The full text the customer got (a text's words, an email's subject
    /// and body): the app shows it when the line is tapped.
    #[must_use]
    pub const fn body(mut self, body: &'a str) -> Self {
        self.body = Some(body);
        self
    }
}

fn default_message(action: &str) -> &'static str {
    match action {
        "created" => "Event created",
        "date_updated" => "Date updated",
        "label_updated" => "Label updated",
        "location_updated" => "Location updated",
        "attachment_added" => "Attachment added",
        "attachment_removed" => "Attachment removed",
        "comment_added" => "Comment added",
        CUSTOMER_MESSAGE => "Message sent to the customer",
        _ => "Event updated",
    }
}

/// Action of a line for an automated text or email that reached the
/// event's customer (a reminder, a flow step, a booking confirmation).
pub const CUSTOMER_MESSAGE: &str = "customer_message";
/// `created_by` of those lines: nobody sent them by hand.
pub const AUTOMATED: &str = "Automated";

/// `Text sent to the customer: Template reminder (1 day before)`.
pub fn customer_message_line(by_text: bool, what: &str) -> String {
    let channel = if by_text { "Text" } else { "Email" };
    format!("{channel} sent to the customer: {what}")
}

/// Notes an automated message the event's customer got.
pub async fn record_customer_message(
    pool: &MySqlPool,
    event_id: i32,
    company_id: i32,
    by_text: bool,
    what: &str,
    body: &str,
) {
    let line = customer_message_line(by_text, what);
    record(
        pool,
        &History::new(event_id, company_id, CUSTOMER_MESSAGE, Some(AUTOMATED))
            .message(&line)
            .body(body),
    )
    .await;
}

/// Adds a line; a failure is only logged (history never blocks a save).
pub async fn record(pool: &MySqlPool, line: &History<'_>) {
    let result = sqlx::query(
        "INSERT INTO event_history (event_id, company_id, action, message, body, from_value,
            to_value, created_by, created_by_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(line.event_id)
    .bind(line.company_id)
    .bind(line.action)
    .bind(line.message.unwrap_or_else(|| default_message(line.action)))
    .bind(line.body.map(str::trim).filter(|body| !body.is_empty()))
    .bind(
        line.from_value
            .map(|value| value.chars().take(500).collect::<String>()),
    )
    .bind(
        line.to_value
            .map(|value| value.chars().take(500).collect::<String>()),
    )
    .bind(line.created_by)
    .bind(line.created_by_id)
    .execute(pool)
    .await;
    if let Err(err) = result {
        tracing::warn!(error = %err, event_id = line.event_id, "event history not saved");
    }
}

#[cfg(test)]
mod tests {
    use super::customer_message_line;

    #[test]
    fn customer_message_lines_name_the_channel() {
        assert_eq!(
            customer_message_line(true, "Template reminder (1 day before)"),
            "Text sent to the customer: Template reminder (1 day before)"
        );
        assert_eq!(
            customer_message_line(false, "Booking confirmation"),
            "Email sent to the customer: Booking confirmation"
        );
    }
}
