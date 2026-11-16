//! The phone pushes a user turned off in the app (Settings › Notifications).
//!
//! Each kind of push has a key ([`SETTINGS`]); a `phone_push_mutes` row
//! (user, key) means that user's phones don't get it. No row means on, so
//! nobody loses a push until they turn it off. [`super::devices::push_to_users`]
//! leaves out the muted users. The 911 alert, quiet refreshes (`leads`) and
//! sign-outs (`revoked`) have no key and always go out.

use super::calls::SALES_MANAGER_POSITION;
use crate::auth::sections::{SHOP_MANAGER_POSITION, SHOP_WORKER_POSITION};
use sqlx::MySqlPool;
use std::collections::BTreeMap;

/// Every push a user can turn off, in the app's order.
pub const SETTINGS: &[&str] = &[
    "missed_call",
    "voicemail",
    "ai_message",
    "text",
    "email",
    "lead_new",
    "lead_repeat",
    "lead_assigned",
    "lead_taken",
    "lead_quote",
    "deal_activity",
    "deal_note",
    "deal_comment",
    "activity_reminder",
    "estimate_reminder",
    "template_reminder",
    "installation_reminder",
    "installation_completed",
    "event_issue",
    "task_assigned",
    "task_due",
    "task_done",
    "step_done",
    "sales_order",
    "ai_promises",
    "scheduled_message",
    "maintenance",
];

/// Only the sales managers get these.
const MANAGER_ONLY: &[&str] = &["lead_new", "lead_repeat", "lead_taken"];

/// The key of a push's `data`; `None` for a push that can't be turned off.
/// A `notification` push is keyed by its row's type
/// ([`setting_of_notification`]), which its data doesn't carry.
pub fn setting_of(data: &BTreeMap<String, String>) -> Option<&'static str> {
    let field = |key: &str| data.get(key).map(String::as_str);
    match field("type")? {
        "missed_call" => Some("missed_call"),
        "voicemail" => Some("voicemail"),
        "ai_message" => Some("ai_message"),
        "text" | "crm_text" => Some("text"),
        "email" => Some("email"),
        "maintenance" => Some("maintenance"),
        "lead" => Some(match field("kind") {
            Some("repeat") => "lead_repeat",
            Some("assigned") => "lead_assigned",
            Some("taken") => "lead_taken",
            Some("quote") => "lead_quote",
            _ => "lead_new",
        }),
        _ => None,
    }
}

/// The key of a `notifications` row of `notification_type`; `None` for a
/// type with no switch.
pub fn setting_of_notification(notification_type: &str) -> Option<&'static str> {
    Some(match notification_type {
        "activity_added" | "activity_edited" | "activity_deleted" => "deal_activity",
        "note_added" | "note_edited" | "note_deleted" => "deal_note",
        "comment_added" | "comment_deleted" => "deal_comment",
        "activity_deadline_reminder" => "activity_reminder",
        "estimate_appointment_reminder" => "estimate_reminder",
        "template_appointment_reminder" => "template_reminder",
        "installation_appointment_reminder" => "installation_reminder",
        "installation_completed" => "installation_completed",
        "event_issue" => "event_issue",
        "project_task_assigned" => "task_assigned",
        "project_task_reminder" => "task_due",
        "project_task_done" => "task_done",
        "project_step_done" => "step_done",
        "sales_order" => "sales_order",
        "delayed_message_sent" | "delayed_message_failed" => "scheduled_message",
        kind if kind.starts_with("ai_promise_") => "ai_promises",
        _ => return None,
    })
}

/// The switches `user_id` of `company_id` gets: the lead switches only
/// sales managers get, maintenance for the shop, promises for those who
/// have them; everything else for everyone.
pub async fn available(
    pool: &MySqlPool,
    user_id: i32,
    company_id: i32,
) -> sqlx::Result<Vec<&'static str>> {
    let (manager, shop, promises): (bool, bool, bool) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM users_positions
                         WHERE user_id = u.id AND company_id = ? AND position_id = ?) <> 0,
                EXISTS (SELECT 1 FROM users_positions
                         WHERE user_id = u.id AND company_id = ? AND position_id IN (?, ?)) <> 0,
                COALESCE(u.ai_promises, 0) <> 0
           FROM users u WHERE u.id = ?",
    )
    .bind(company_id)
    .bind(SALES_MANAGER_POSITION)
    .bind(company_id)
    .bind(SHOP_MANAGER_POSITION)
    .bind(SHOP_WORKER_POSITION)
    .bind(user_id)
    .fetch_optional(pool)
    .await?
    .unwrap_or_default();
    Ok(SETTINGS
        .iter()
        .copied()
        .filter(|key| match *key {
            key if MANAGER_ONLY.contains(&key) => manager,
            "maintenance" => shop,
            "ai_promises" => promises,
            _ => true,
        })
        .collect())
}

/// The keys `user_id` turned off.
pub async fn muted(pool: &MySqlPool, user_id: i32) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar("SELECT push_key FROM phone_push_mutes WHERE user_id = ? ORDER BY push_key")
        .bind(user_id)
        .fetch_all(pool)
        .await
}

/// Turns push `key` on or off for `user_id`.
pub async fn set(pool: &MySqlPool, user_id: i32, key: &str, on: bool) -> sqlx::Result<()> {
    if on {
        sqlx::query("DELETE FROM phone_push_mutes WHERE user_id = ? AND push_key = ?")
            .bind(user_id)
            .bind(key)
            .execute(pool)
            .await?;
    } else {
        sqlx::query("INSERT IGNORE INTO phone_push_mutes (user_id, push_key) VALUES (?, ?)")
            .bind(user_id)
            .bind(key)
            .execute(pool)
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn keys_pushes() {
        assert_eq!(setting_of(&data(&[("type", "crm_text")])), Some("text"));
        assert_eq!(
            setting_of(&data(&[("type", "lead"), ("kind", "assigned")])),
            Some("lead_assigned")
        );
        assert_eq!(
            setting_of(&data(&[("type", "lead"), ("kind", "new")])),
            Some("lead_new")
        );
        assert_eq!(setting_of(&data(&[("type", "e911_alert")])), None);
        assert_eq!(setting_of(&data(&[("type", "leads")])), None);
        assert_eq!(setting_of(&data(&[("type", "revoked")])), None);
        assert_eq!(setting_of(&data(&[("type", "notification")])), None);
    }

    #[test]
    fn keys_notifications() {
        assert_eq!(setting_of_notification("note_edited"), Some("deal_note"));
        assert_eq!(
            setting_of_notification("ai_promise_digest"),
            Some("ai_promises")
        );
        assert_eq!(
            setting_of_notification("delayed_message_failed"),
            Some("scheduled_message")
        );
        assert_eq!(setting_of_notification("something_new"), None);
    }

    #[test]
    fn every_key_is_listed() {
        for kind in [
            "missed_call",
            "voicemail",
            "ai_message",
            "text",
            "email",
            "maintenance",
        ] {
            let key = setting_of(&data(&[("type", kind)])).unwrap();
            assert!(SETTINGS.contains(&key), "{key}");
        }
        for kind in [
            "activity_added",
            "project_task_done",
            "sales_order",
            "event_issue",
        ] {
            let key = setting_of_notification(kind).unwrap();
            assert!(SETTINGS.contains(&key), "{key}");
        }
    }
}
