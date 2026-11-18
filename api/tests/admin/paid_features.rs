use crate::common;

use api::app;
use api::state::AppState;
use axum::http::StatusCode;
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Ada Admin (102) is made a superuser; company 100 is Acme Stone, 101 Other
// Granite Co.

async fn server(pool: &MySqlPool) -> TestServer {
    sqlx::query("UPDATE users SET is_superuser = 1 WHERE id = 102")
        .execute(pool)
        .await
        .unwrap();
    TestServer::new(app(AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)))
}

fn feature_id(body: &Value, name: &str) -> i64 {
    body["features"]
        .as_array()
        .unwrap()
        .iter()
        .find(|feature| feature["name"] == name)
        .unwrap_or_else(|| panic!("no feature {name}"))["id"]
        .as_i64()
        .unwrap()
}

fn usage(body: &Value, company_id: i64, metric: &str) -> Option<i64> {
    body["usage"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["company_id"] == company_id && row["metric"] == metric)
        .map(|row| row["count"].as_i64().unwrap())
}

#[test_db::test(seed)]
async fn superusers_only(pool: MySqlPool) {
    let server = server(&pool).await;
    server
        .get("/v1/superuser/paid-features")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .put("/v1/superuser/companies/100/paid-features/1")
        .with_session(sessions::MANAGER)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .post("/v1/superuser/paid-features")
        .with_session(sessions::REP)
        .json(&json!({ "name": "Hack" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn lists_seeded_features_and_counts_usage(pool: MySqlPool) {
    // Two Chat questions this month and one older, all at Acme.
    sqlx::query(
        "INSERT INTO chat_log (user_id, company_id, question, answer, created_at) VALUES
           (102, 100, 'q1', 'a', NOW()),
           (102, 100, 'q2', 'a', NOW() - INTERVAL 3 DAY),
           (102, 100, 'q3', 'a', NOW() - INTERVAL 40 DAY)",
    )
    .execute(&pool)
    .await
    .unwrap();
    // Two texts sent by Acme, and one that belongs to no company (the column
    // allows it): it is left out, it does not break the list.
    sqlx::query(
        "INSERT INTO telnyx_sms (recipient, text, direction, status, company_id) VALUES
           (13175550001, 't1', 'outbound', 'sent', 100),
           (13175550001, 't2', 'outbound', 'sent', 100),
           (13175550001, 't3', 'outbound', 'sent', NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(&pool).await;
    let response = server
        .get("/v1/superuser/paid-features")
        .with_session(sessions::ADMIN)
        .await;
    response.assert_status_ok();
    let body: Value = response.json();
    // Every seeded feature names a metric the API knows, so every metric's
    // query ran against the migrated schema.
    let metrics: Vec<&str> = body["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|metric| metric["key"].as_str().unwrap())
        .collect();
    assert_eq!(metrics.len(), 10);
    for feature in body["features"].as_array().unwrap() {
        let metric = feature["usage_metric"].as_str().unwrap();
        assert!(metrics.contains(&metric), "unknown metric {metric}");
    }
    assert_eq!(body["usage_days"], 30);
    assert_eq!(usage(&body, 100, "chat_questions"), Some(2));
    assert_eq!(usage(&body, 101, "chat_questions"), None);
    assert_eq!(usage(&body, 100, "texts_sent"), Some(2));
    assert_eq!(body["paid"], json!([]));
}

#[test_db::test(seed)]
async fn marks_and_unmarks_paid(pool: MySqlPool) {
    let server = server(&pool).await;
    let body: Value = server
        .get("/v1/superuser/paid-features")
        .with_session(sessions::ADMIN)
        .await
        .json();
    let id = feature_id(&body, "E-signatures");
    let path = format!("/v1/superuser/companies/100/paid-features/{id}");
    // Marking twice is fine.
    for _ in 0..2 {
        server
            .put(&path)
            .with_session(sessions::ADMIN)
            .await
            .assert_status(StatusCode::NO_CONTENT);
    }
    let body: Value = server
        .get("/v1/superuser/paid-features")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(
        body["paid"],
        json!([{ "company_id": 100, "feature_id": id }])
    );
    // No such company or feature.
    server
        .put(&format!(
            "/v1/superuser/companies/999999/paid-features/{id}"
        ))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .put("/v1/superuser/companies/100/paid-features/999999")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    // Unmarking twice is fine.
    for _ in 0..2 {
        server
            .delete(&path)
            .with_session(sessions::ADMIN)
            .await
            .assert_status(StatusCode::NO_CONTENT);
    }
    let body: Value = server
        .get("/v1/superuser/paid-features")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(body["paid"], json!([]));
}

#[test_db::test(seed)]
async fn adds_renames_and_removes_features(pool: MySqlPool) {
    let server = server(&pool).await;
    let response = server
        .post("/v1/superuser/paid-features")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "  Leaderboard  " }))
        .await;
    response.assert_status(StatusCode::CREATED);
    let created: Value = response.json();
    assert_eq!(created["name"], "Leaderboard");
    assert_eq!(created["usage_metric"], Value::Null);
    let id = created["id"].as_i64().unwrap();

    // Taken name, empty name, unknown metric.
    server
        .post("/v1/superuser/paid-features")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Leaderboard" }))
        .await
        .assert_status(StatusCode::CONFLICT);
    server
        .post("/v1/superuser/paid-features")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": " " }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .post("/v1/superuser/paid-features")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "X", "usage_metric": "nope" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    server
        .put(&format!("/v1/superuser/paid-features/{id}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Points leaderboard", "usage_metric": "chat_questions" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .put("/v1/superuser/paid-features/999999")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Ghost" }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .put(&format!("/v1/superuser/companies/100/paid-features/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let body: Value = server
        .get("/v1/superuser/paid-features")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(feature_id(&body, "Points leaderboard"), id);

    // Removing a feature removes its marks.
    server
        .delete(&format!("/v1/superuser/paid-features/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .delete(&format!("/v1/superuser/paid-features/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    let marks: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM company_features WHERE feature_id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(marks, 0);
}
