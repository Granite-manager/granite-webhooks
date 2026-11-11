//! Issues reported on an event (`routes/events/issues.rs`): a description
//! and pictures, listed by the event's details, and the sales rep is told.

use crate::common;

use axum::http::StatusCode;
use common::{WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// Event 8970 of company 100: made by Mark (101), customer Alice (1000),
/// whose sales rep is Rita (100). Event 8971 is company 101's.
const EVENT: &str = "
INSERT INTO calendars (id, company_id, name, color) VALUES (8813, 100, 'Crew', 'teal');
INSERT INTO events (id, title, location, start_date, end_date, created_user_id, assigned_user_id, calendar_id, customer_id) VALUES
  (8970, 'Kitchen install', '9 Elm St', '2026-11-02 15:00:00', '2026-11-02 16:00:00', 101, 101, 8813, 1000),
  (8971, 'Their event', NULL, '2026-11-02 15:00:00', '2026-11-02 16:00:00', 106, NULL, NULL, NULL);
";

async fn issues(server: &axum_test::TestServer, session: &str) -> Vec<Value> {
    let details: Value = server
        .get("/v1/events/8970/details")
        .with_session(session)
        .await
        .json();
    details["issues"].as_array().unwrap().clone()
}

#[test_db::test(seed)]
async fn an_issue_has_a_description_and_pictures_and_tells_the_rep(pool: MySqlPool) {
    sqlx::raw_sql(EVENT).execute(&pool).await.unwrap();
    let (server, storage) = common::server_with_storage(pool.clone());
    assert!(issues(&server, sessions::REP).await.is_empty());

    let res = server
        .post("/v1/uploads")
        .with_session(sessions::MANAGER)
        .json(&json!({ "folder": "events", "file_name": "crack.png", "content_type": "image/png" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let url = res.json::<Value>()["url"].as_str().unwrap().to_string();
    storage
        .put(
            &storage.key_for_url(&url).unwrap(),
            common::png(8, 8),
            "image/png",
        )
        .await
        .unwrap();

    // What is wrong must be said, within 2000 characters, with uploaded pictures only.
    for body in [
        json!({ "description": "   " }),
        json!({ "description": "x".repeat(2001) }),
        json!({ "description": "Cracked", "urls": ["https://elsewhere.test/x.png"] }),
    ] {
        server
            .post("/v1/events/8970/issues")
            .with_session(sessions::MANAGER)
            .json(&body)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    assert!(issues(&server, sessions::REP).await.is_empty());

    server
        .post("/v1/events/8970/issues")
        .with_session(sessions::MANAGER)
        .json(&json!({ "description": "  The slab cracked at the sink cutout ", "urls": [url] }))
        .await
        .assert_status(StatusCode::CREATED);
    let listed = issues(&server, sessions::REP).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0]["description"],
        "The slab cracked at the sink cutout"
    );
    assert_eq!(listed[0]["created_by_id"], 101);
    assert_eq!(listed[0]["photos"][0]["url"], url.as_str());

    // Alice's rep (100) is told, with the event to open.
    let told: Vec<(i32, Option<i32>, Option<i32>, String)> = sqlx::query_as(
        "SELECT user_id, customer_id, event_id, message FROM notifications
          WHERE notification_type = 'event_issue' ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        told,
        vec![(
            100,
            Some(1000),
            Some(8970),
            "Issue on Alice Lead: The slab cracked at the sink cutout".to_string()
        )]
    );
    let bell: Value = server
        .get("/v1/notifications")
        .with_session(sessions::REP)
        .await
        .json();
    let item = bell["notifications"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["notification_type"] == "event_issue")
        .expect("the bell lists it");
    assert_eq!(item["type_title"], "Reported an Issue");
    assert_eq!(item["event_id"], 8970);

    // The rep reporting one themselves is not told about it.
    server
        .post("/v1/events/8970/issues")
        .with_session(sessions::REP)
        .json(&json!({ "description": "Wrong faucet on site" }))
        .await
        .assert_status(StatusCode::CREATED);
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM notifications WHERE notification_type = 'event_issue'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    assert_eq!(issues(&server, sessions::REP).await.len(), 2);
}

#[test_db::test(seed)]
async fn an_issue_is_deleted_by_its_reporter_or_an_admin_within_the_company(pool: MySqlPool) {
    sqlx::raw_sql(EVENT).execute(&pool).await.unwrap();
    let server = common::server(pool.clone());
    // Another company's event is not found.
    server
        .post("/v1/events/8971/issues")
        .with_session(sessions::REP)
        .json(&json!({ "description": "Not mine" }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    for text in ["First", "Second"] {
        server
            .post("/v1/events/8970/issues")
            .with_session(sessions::MANAGER)
            .json(&json!({ "description": text }))
            .await
            .assert_status(StatusCode::CREATED);
    }
    let listed = issues(&server, sessions::REP).await;
    let (first, second) = (listed[0]["id"].clone(), listed[1]["id"].clone());
    // Not the reporter, not an admin.
    server
        .delete(&format!("/v1/events/8970/issues/{first}"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .delete(&format!("/v1/events/8970/issues/{first}"))
        .with_session(sessions::MANAGER)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .delete(&format!("/v1/events/8970/issues/{second}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(issues(&server, sessions::REP).await.is_empty());
}
