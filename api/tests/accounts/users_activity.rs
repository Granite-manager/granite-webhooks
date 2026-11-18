use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::Value;
use sqlx::MySqlPool;

/// Recent activity of company 100: a high-priority deal activity and a note
/// on deal 8002 (Erin), a text from Rita (agent A1) and one through Telnyx to Alice's number, an
/// email from Rita to Alice's address, a new deal for Alice.
async fn seed(pool: &MySqlPool) {
    sqlx::raw_sql(
        "UPDATE users SET cloudtalk_agent_id = 'A1' WHERE id = 100;
         UPDATE company SET leaderboard_enabled = 1 WHERE id = 100;
         INSERT INTO deal_activities (id, deal_id, company_id, name, priority, created_at, created_by, deadline)
           VALUES (9800, 8002, 100, 'Call back', 'high', NOW() - INTERVAL 2 HOUR, 'Rita Rep', NOW() + INTERVAL 1 DAY);
         INSERT INTO deal_notes (id, deal_id, company_id, content, created_at)
           VALUES (9801, 8002, 100, '\n  Measured the island \nsecond line', NOW() - INTERVAL 1 HOUR);
         INSERT INTO cloudtalk_sms (id, sender, recipient, text, direction, status, agent, created_date, company_id, is_echo, is_automated)
           VALUES (9802, 13175559999, 13175550100, 'See you Monday', 'outbound', 'sent', 'A1', NOW() - INTERVAL 30 MINUTE, 100, 0, 0);
         INSERT INTO telnyx_sms (id, sender, recipient, text, direction, status, agent, sender_user_id, created_date, company_id, recipient10, phone_digits)
           VALUES (9805, 13175558888, 13175550100, 'Sent from Granite Manager', 'outbound', 'sent', '100', 100, NOW() - INTERVAL 20 MINUTE, 100, '3175550100', '3175550100');
         INSERT INTO telnyx_sms (id, sender, recipient, text, direction, status, agent, sender_user_id, created_date, company_id, is_automated)
           VALUES (9806, 13175558888, 13175550100, 'Flow step', 'outbound', 'sent', '100', 100, NOW() - INTERVAL 15 MINUTE, 100, 1);
         INSERT INTO emails (id, thread_id, subject, body, sender_user_id, sender_email, receiver_email, sent_at, company_id, is_draft)
           VALUES (9803, 't-9803', '  ', 'Hi', 100, 'rep@acme.test', 'Alice <ALICE@example.com>', NOW() - INTERVAL 10 MINUTE, 100, 0);
         INSERT INTO deals (id, customer_id, amount, title, list_id, position, user_id, created_at, updated_at, created_by, description)
           VALUES (9804, 1000, 1234.5, 'New one', 101, 9, 100, NOW() - INTERVAL 5 MINUTE, NOW(), 'Rita Rep', 'Wants quartz\nsoon');",
    )
    .execute(pool)
    .await
    .unwrap();
}

fn find<'a>(activities: &'a Value, source: &str, id: i64) -> &'a Value {
    activities
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["source"] == source && a["id"] == id)
        .unwrap_or_else(|| panic!("no {source} {id}"))
}

#[test_db::test(seed)]
async fn admins_see_what_users_did(pool: MySqlPool) {
    seed(&pool).await;
    let server = server(pool.clone());
    server
        .get("/v1/users/activity")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let body: Value = server
        .get("/v1/users/activity")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert!(body["activities"].is_array(), "{body}");
    let activities = &body["activities"];
    // Newest first.
    let dates: Vec<&str> = activities
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["created_at"].as_str())
        .collect();
    assert!(dates.windows(2).all(|pair| pair[0] >= pair[1]));

    let task = find(activities, "deal_activity", 9800);
    assert_eq!(task["user_name"], "Rita Rep");
    assert_eq!(task["tags"], serde_json::json!([]));
    assert!(task["due_at"].is_string());
    let note = find(activities, "deal_note", 9801);
    assert_eq!(note["summary"], "Measured the island");
    let text = find(activities, "cloudtalk_sms", 9802);
    assert_eq!(text["customer_name"], "Alice Lead");
    assert_eq!(text["contact"], "13175550100");
    assert_eq!(text["user_id"], 100);
    // A text sent through Granite Manager's own texting (Telnyx) counts too.
    let telnyx = find(activities, "telnyx_sms", 9805);
    assert_eq!(telnyx["action"], "Sent a text");
    assert_eq!(telnyx["customer_name"], "Alice Lead");
    assert_eq!(telnyx["contact"], "13175550100");
    assert_eq!(telnyx["user_id"], 100);
    // A follow-up flow's text is not the rep's doing.
    assert!(
        !activities
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["source"] == "telnyx_sms" && a["id"] == 9806)
    );
    let email = find(activities, "email", 9803);
    assert_eq!(email["customer_name"], "Alice Lead");
    assert_eq!(email["summary"], "(no subject)");
    let deal = find(activities, "deal_created", 9804);
    assert_eq!(deal["summary"], "$1,235 · Wants quartz");
    assert_eq!(deal["tags"], serde_json::json!(["Quoted", "$1,235"]));
    assert_eq!(deal["user_id"], 100);

    assert_eq!(common::ids(&body["sales_reps"]), [100]);
    // The Telnyx text scores in the "Calls & SMS points" table: without it
    // Rita has one text fewer.
    let texts_of_rita = |body: &Value| {
        let rows = body["call_sms_points"].as_array().unwrap();
        let rita = rows.iter().find(|row| row["user_id"] == 100).unwrap();
        rita["sms"].as_i64().unwrap()
    };
    let with_telnyx = texts_of_rita(&body);
    sqlx::query("DELETE FROM telnyx_sms WHERE id = 9805")
        .execute(&pool)
        .await
        .unwrap();
    let body: Value = server
        .get("/v1/users/activity")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(texts_of_rita(&body), with_telnyx - 1);
    server
        .get("/v1/users/activity?calls_from=2026-05-02&calls_to=2026-05-01")
        .with_session(sessions::ADMIN)
        .await
        .assert_status_bad_request();
}
