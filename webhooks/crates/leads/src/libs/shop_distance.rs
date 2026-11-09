//! The "Distance" line of a lead's Telegram message: driving miles from the
//! company's address to the customer's.

use lambda_http::tracing;
use sqlx::MySqlPool;
use std::time::Duration;
use webhooks_phone::google::maps::driving_distance_miles;

/// Longest the lead message waits for Google.
const DISTANCE_TIMEOUT: Duration = Duration::from_secs(5);

/// The customer's address as one line: street, city and zip, skipping empty
/// parts and parts the street already names.
fn customer_destination(
    address: Option<&str>,
    city: Option<&str>,
    postal_code: Option<&str>,
) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in [address, city, postal_code].into_iter().flatten() {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let lower = part.to_lowercase();
        if parts.iter().any(|p| p.to_lowercase().contains(&lower)) {
            continue;
        }
        parts.push(part);
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

fn distance_line(miles: f64) -> String {
    format!("Distance: {miles:.1} mi from the shop")
}

/// "Distance: 12.3 mi from the shop", or `None` when the company or the
/// customer has no address, there is no Maps key, or Google fails.
pub async fn shop_distance_line(
    pool: &MySqlPool,
    company_id: i32,
    customer_id: u64,
) -> Option<String> {
    if std::env::var("GOOGLE_MAPS_API_KEY")
        .ok()
        .is_none_or(|key| key.trim().is_empty())
    {
        return None;
    }
    let row: Option<(
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    )> = sqlx::query_as(
        "SELECT c.address, c.city, c.postal_code, co.address
             FROM customers c JOIN company co ON co.id = c.company_id
             WHERE c.id = ? AND c.company_id = ?",
    )
    .bind(customer_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    let (address, city, postal_code, shop) = row?;
    let shop = shop
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    let destination =
        customer_destination(address.as_deref(), city.as_deref(), postal_code.as_deref())?;
    match tokio::time::timeout(
        DISTANCE_TIMEOUT,
        driving_distance_miles(&shop, &destination),
    )
    .await
    {
        Ok(Ok(miles)) => Some(distance_line(miles)),
        Ok(Err(error)) => {
            tracing::warn!(?error, customer_id, "Lead distance from the shop failed");
            None
        }
        Err(_) => {
            tracing::warn!(customer_id, "Lead distance from the shop timed out");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_joins_the_parts_it_has() {
        assert_eq!(
            customer_destination(Some("1 Main St"), Some("Indianapolis"), Some("46235")),
            Some("1 Main St, Indianapolis, 46235".to_string())
        );
        assert_eq!(
            customer_destination(None, Some("Indianapolis"), Some("46235")),
            Some("Indianapolis, 46235".to_string())
        );
        assert_eq!(
            customer_destination(
                Some("1 Main St, Indianapolis, IN 46235"),
                None,
                Some("46235")
            ),
            Some("1 Main St, Indianapolis, IN 46235".to_string())
        );
        assert_eq!(customer_destination(Some(" "), None, Some("")), None);
    }

    #[test]
    fn line_shows_one_decimal() {
        assert_eq!(distance_line(12.345), "Distance: 12.3 mi from the shop");
    }
}
