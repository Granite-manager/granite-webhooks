//! Customer to-dos (`routes/customer_todos.rs`): admins keep the lists, the
//! template maker sends what applies from the appointment, the sales rep is
//! told, and the to-dos show on the customer, the appointment and the project.

use crate::common;

use axum::http::StatusCode;
use common::{WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// Event 8980 of company 100: made by Mark (101), customer Alice (1000),
/// whose sales rep is Rita (100). Event 8981 has no customer. Event 8982 is
/// Erin Builder's (1004), whose deal 8002 becomes a project.
const EVENTS: &str = "
INSERT INTO calendars (id, company_id, name, color) VALUES (8814, 100, 'Templates', 'teal');
INSERT INTO events (id, title, location, start_date, end_date, created_user_id, assigned_user_id, calendar_id, customer_id) VALUES
  (8980, 'Template', '9 Elm St', '2026-11-02 15:00:00', '2026-11-02 16:00:00', 101, 101, 8814, 1000),
  (8981, 'No customer', NULL, '2026-11-02 15:00:00', '2026-11-02 16:00:00', 101, 101, 8814, NULL),
  (8982, 'Erin template', NULL, '2026-11-03 15:00:00', '2026-11-03 16:00:00', 101, 101, 8814, 1004);
";

async fn details_todos(server: &axum_test::TestServer, event: i32) -> Value {
    let details: Value = server
        .get(&format!("/v1/events/{event}/details"))
        .with_session(sessions::REP)
        .await
        .json();
    details["customer_todos"].clone()
}

#[test_db::test(seed)]
async fn admins_keep_the_lists_and_everyone_reads_them(pool: MySqlPool) {
    let server = common::server(pool.clone());
    server
        .post("/v1/customer-checklists")
        .with_session(sessions::REP)
        .json(&json!({ "name": "Before install", "items": ["Level the cabinets"] }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .post("/v1/customer-checklists")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "  ", "items": [] }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let added = server
        .post("/v1/customer-checklists")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": " Before install ", "items": ["Level the cabinets", " ", "Bring the sink to the shop"] }))
        .await;
    added.assert_status(StatusCode::CREATED);
    let lists: Value = added.json();
    assert_eq!(lists[0]["name"], "Before install");
    assert_eq!(lists[0]["items"].as_array().unwrap().len(), 2);
    let id = lists[0]["id"].as_i64().unwrap();

    let saved: Value = server
        .put(&format!("/v1/customer-checklists/{id}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Before install", "items": ["Bring the sink to the shop", "Install overhang supports"] }))
        .await
        .json();
    assert_eq!(saved[0]["items"][0]["label"], "Bring the sink to the shop");
    assert_eq!(saved[0]["items"][1]["label"], "Install overhang supports");

    // Anyone in the company reads them; another company does not.
    let read: Value = server
        .get("/v1/customer-checklists")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(read, saved);
    let theirs: Value = server
        .get("/v1/customer-checklists")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json();
    assert_eq!(theirs, json!([]));

    server
        .delete(&format!("/v1/customer-checklists/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let left: Value = server
        .get("/v1/customer-checklists")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(left, json!([]));
}

#[test_db::test(seed)]
async fn the_template_maker_sends_to_dos_and_the_rep_sees_and_ticks_them(pool: MySqlPool) {
    sqlx::raw_sql(EVENTS).execute(&pool).await.unwrap();
    let server = common::server(pool.clone());
    assert!(details_todos(&server, 8980).await["sent_at"].is_null());

    // An appointment with no customer takes none.
    server
        .post("/v1/events/8981/customer-todos")
        .with_session(sessions::MANAGER)
        .json(&json!({ "items": [{ "label": "Level the cabinets" }] }))
        .await
        .assert_status(StatusCode::CONFLICT);

    let sent = server
        .post("/v1/events/8980/customer-todos")
        .with_session(sessions::MANAGER)
        .json(&json!({ "items": [
            { "label": "Install overhang supports", "note": " Island, 2 corbels " },
            { "label": "Bring the sink to the shop", "note": " " },
        ] }))
        .await;
    sent.assert_status(StatusCode::CREATED);
    let sent: Value = sent.json();
    assert_eq!(sent["nothing_to_do"], false);
    assert_eq!(sent["todos"][0]["note"], "Island, 2 corbels");
    assert!(sent["todos"][1]["note"].is_null());
    assert!(sent["sent_at"].is_string());

    // Alice's rep (100) is told once, with the event to open.
    let told: Vec<(i32, Option<i32>, Option<i32>, String)> = sqlx::query_as(
        "SELECT user_id, customer_id, event_id, message FROM notifications
          WHERE notification_type = 'customer_todos'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        told,
        vec![(
            100,
            Some(1000),
            Some(8980),
            "Alice Lead has 2 to-dos from the template".to_string()
        )]
    );

    // The customer's page lists them; the rep ticks one off.
    let listed: Value = server
        .get("/v1/customers/1000/customer-todos")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(listed.as_array().unwrap().len(), 2);
    let first = listed[0]["id"].as_i64().unwrap();
    server
        .put(&format!("/v1/customer-todos/{first}"))
        .with_session(sessions::REP)
        .json(&json!({ "done": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let listed: Value = server
        .get("/v1/customers/1000/customer-todos")
        .with_session(sessions::REP)
        .await
        .json();
    // Open ones first.
    assert_eq!(listed[0]["label"], "Bring the sink to the shop");
    assert_eq!(listed[1]["done_by"], "Rita Rep");
    server
        .get("/v1/customers/1000/customer-todos")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);

    // Only the sender or an admin deletes one.
    server
        .delete(&format!("/v1/customer-todos/{first}"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .delete(&format!("/v1/customer-todos/{first}"))
        .with_session(sessions::MANAGER)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(details_todos(&server, 8980).await["todos"].as_array().unwrap().len(), 1);
}

#[test_db::test(seed)]
async fn nothing_to_do_is_kept_and_tells_no_one(pool: MySqlPool) {
    sqlx::raw_sql(EVENTS).execute(&pool).await.unwrap();
    let server = common::server(pool.clone());
    server
        .post("/v1/events/8980/customer-todos")
        .with_session(sessions::MANAGER)
        .json(&json!({ "items": [] }))
        .await
        .assert_status(StatusCode::CREATED);
    let todos = details_todos(&server, 8980).await;
    assert_eq!(todos["nothing_to_do"], true);
    assert!(todos["sent_at"].is_string());
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM notifications WHERE notification_type = 'customer_todos'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 0);
}

#[test_db::test(seed)]
async fn the_project_shows_its_customers_to_dos(pool: MySqlPool) {
    sqlx::raw_sql(EVENTS).execute(&pool).await.unwrap();
    sqlx::raw_sql("UPDATE company SET auto_project = 1 WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    let server = common::server(pool.clone());
    server
        .put("/v1/deals/8002/outcome")
        .with_session(sessions::ADMIN)
        .json(&json!({ "outcome": "won" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let project: i32 = sqlx::query_scalar("SELECT id FROM projects WHERE source_deal_id = 8002")
        .fetch_one(&pool)
        .await
        .unwrap();
    server
        .post("/v1/events/8982/customer-todos")
        .with_session(sessions::MANAGER)
        .json(&json!({ "items": [{ "label": "Level the cabinets" }] }))
        .await
        .assert_status(StatusCode::CREATED);
    let page: Value = server
        .get(&format!("/v1/projects/{project}"))
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(page["customer_todos"][0]["label"], "Level the cabinets");
    assert_eq!(page["customer_todos"][0]["project_id"], project);
}
