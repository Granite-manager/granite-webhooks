//! Admin › Deals › Automatic Activities: when a deal moves from one list to
//! another, the company's matching rules (`deal_stage_activity_rules`) add an
//! activity to the deal, due a set time later, with a deadline reminder for
//! the deal's sales rep.

use chrono::{DateTime, Duration, NaiveTime, Timelike, Utc};
use chrono_tz::America::Indiana::Indianapolis;
use sqlx::MySqlConnection;

/// Deadline of an activity added at `now`, due `amount` `unit` later, as the
/// store wall time `deal_activities.deadline` holds. Midnight means "a whole
/// day" there, so an exact midnight moves to 00:01.
pub fn due_wall_time(now: DateTime<Utc>, amount: i32, unit: &str) -> String {
    let span = if unit == "days" {
        Duration::days(amount.into())
    } else {
        Duration::hours(amount.into())
    };
    let mut wall = (now + span)
        .with_timezone(&Indianapolis)
        .naive_local()
        .with_second(0)
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or_else(|| (now + span).naive_utc());
    if wall.time() == NaiveTime::MIN {
        wall += Duration::minutes(1);
    }
    wall.format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// Adds the activities of the rules matching a move of `deal_id` from
/// `from_list_id` to `to_list_id`. A rule whose last activity on this deal is
/// still open adds nothing, so moving back and forth makes no duplicates.
pub async fn add_for_move(
    conn: &mut MySqlConnection,
    deal_id: i64,
    from_list_id: i32,
    to_list_id: i32,
) -> Result<(), sqlx::Error> {
    if from_list_id == to_list_id {
        return Ok(());
    }
    let rules: Vec<(i32, i32, String, i32, String, Option<i32>)> = sqlx::query_as(
        "SELECT r.id, c.company_id, r.activity_name, r.due_amount, CAST(r.due_unit AS CHAR), d.user_id
           FROM deals d
           JOIN customers c ON c.id = d.customer_id
           JOIN deal_stage_activity_rules r
             ON r.company_id = c.company_id AND r.to_list_id = ?
            AND (r.from_list_id IS NULL OR r.from_list_id = ?)
            AND r.is_enabled = 1 AND r.deleted_at IS NULL
          WHERE d.id = ? AND d.deleted_at IS NULL
            AND NOT EXISTS (
                SELECT 1 FROM deal_activities a
                 WHERE a.deal_id = d.id AND a.stage_rule_id = r.id
                   AND a.is_completed = 0 AND a.deleted_at IS NULL)
          ORDER BY r.id",
    )
    .bind(to_list_id)
    .bind(from_list_id)
    .bind(deal_id)
    .fetch_all(&mut *conn)
    .await?;
    let now = Utc::now();
    for (rule_id, company_id, name, amount, unit, owner) in rules {
        let deadline = due_wall_time(now, amount, &unit);
        sqlx::query(
            "INSERT INTO deal_activities
               (deal_id, company_id, name, deadline, priority, created_by, stage_rule_id)
             VALUES (?, ?, ?, ?, 'medium', 'Automatic', ?)",
        )
        .bind(deal_id)
        .bind(company_id)
        .bind(&name)
        .bind(&deadline)
        .bind(rule_id)
        .execute(&mut *conn)
        .await?;
        let Some(owner) = owner else { continue };
        let due_at = now
            + if unit == "days" {
                Duration::days(amount.into())
            } else {
                Duration::hours(amount.into())
            };
        sqlx::query(
            "INSERT INTO notifications (user_id, deal_id, message, notification_type, actor_name, due_at)
             VALUES (?, ?, ?, 'activity_deadline_reminder', NULL, ?)",
        )
        .bind(owner)
        .bind(deal_id)
        .bind(name.chars().take(255).collect::<String>())
        .bind(due_at.naive_utc())
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn due_in_hours_is_store_wall_time() {
        // 2026-11-14 19:15 UTC = 14:15 in Indianapolis (EST).
        let now = Utc.with_ymd_and_hms(2026, 11, 14, 19, 15, 42).unwrap();
        assert_eq!(due_wall_time(now, 24, "hours"), "2026-11-15T14:15:00");
        assert_eq!(due_wall_time(now, 2, "days"), "2026-11-16T14:15:00");
    }

    #[test]
    fn midnight_is_not_a_whole_day() {
        let now = Utc.with_ymd_and_hms(2026, 11, 14, 5, 0, 0).unwrap(); // 00:00 EST
        assert_eq!(due_wall_time(now, 1, "days"), "2026-11-15T00:01:00");
    }
}
