use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::Value;
use sqlx::MySqlPool;

// Seed: company 100. Rita (100) is its sales rep, Mark (101) its sales
// manager. Leads here are made now, so they fall in the 14-day window.

async fn seed_leads(pool: &MySqlPool) {
    sqlx::query(
        "INSERT INTO customers (id, name, phone, company_id, source, sales_rep, created_date, assigned_date, invalid_lead) VALUES
          (9100, 'New Unassigned', '317-555-9100', 100, 'leads',    NULL, NOW() - INTERVAL 1 HOUR, NULL, NULL),
          (9101, 'Rita New',       '317-555-9101', 100, 'leads',    100,  NOW() - INTERVAL 2 HOUR, NOW(), NULL),
          (9102, 'Mark Own',       NULL,           100, 'leads',    101,  NOW() - INTERVAL 3 HOUR, NOW() - INTERVAL 1 HOUR, NULL),
          (9103, 'Old Unassigned', NULL,           100, 'leads',    NULL, NOW() - INTERVAL 30 DAY, NULL, NULL),
          (9104, 'Spam Lead',      NULL,           100, 'leads',    NULL, NOW(), NULL, 'spam'),
          (9105, 'Walk In',        NULL,           100, 'check-in', NULL, NOW(), NULL, NULL),
          (9106, 'Rita Old',       NULL,           100, 'leads',    100,  NOW() - INTERVAL 40 DAY, NOW() - INTERVAL 20 DAY, NULL)",
    )
    .execute(pool)
    .await
    .unwrap();
}

fn ids(list: &Value) -> Vec<i64> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|lead| lead["customer_id"].as_i64().unwrap())
        .collect()
}

#[test_db::test(seed)]
async fn a_rep_sees_only_their_recent_leads(pool: MySqlPool) {
    seed_leads(&pool).await;
    let res = server(pool)
        .get("/v1/customers/lead-inbox")
        .with_session(sessions::REP)
        .await;
    res.assert_status(StatusCode::OK);
    let inbox: Value = res.json();
    assert_eq!(inbox["is_manager"], false);
    assert_eq!(ids(&inbox["assigned"]), [9101]);
    assert_eq!(inbox["assigned"][0]["sales_rep_name"], "Rita Rep");
    assert_eq!(ids(&inbox["unassigned"]), Vec::<i64>::new());
    assert_eq!(inbox["reps"], serde_json::json!([]));
    // A rep still sees how many leads each sales rep got this month.
    let team = inbox["team"].as_array().unwrap();
    assert_eq!(team.len(), 1);
    assert_eq!(team[0]["id"], 100);
    assert_eq!(team[0]["name"], "Rita Rep");
    assert!(team[0]["month_leads"].as_i64().unwrap() >= 1);
}

#[test_db::test(seed)]
async fn a_manager_sees_unassigned_leads_and_the_reps(pool: MySqlPool) {
    seed_leads(&pool).await;
    let res = server(pool)
        .get("/v1/customers/lead-inbox")
        .with_session(sessions::MANAGER)
        .await;
    res.assert_status(StatusCode::OK);
    let inbox: Value = res.json();
    assert_eq!(inbox["is_manager"], true);
    assert_eq!(ids(&inbox["unassigned"]), [9100]);
    assert_eq!(inbox["unassigned"][0]["deal_id"], Value::Null);
    assert_eq!(ids(&inbox["assigned"]), [9101, 9102]);
    let reps = inbox["reps"].as_array().unwrap();
    assert_eq!(reps.len(), 1);
    assert_eq!(reps[0]["id"], 100);
    assert!(reps[0]["month_leads"].as_i64().unwrap() >= 1);
    assert_eq!(inbox["team"], inbox["reps"]);
}

// ---- the website's Leads page (Deals › Leads, 2026-11-18) ----------------

#[test_db::test(seed)]
async fn a_manager_gets_the_lead_details_and_the_pipelines(pool: MySqlPool) {
    seed_leads(&pool).await;
    sqlx::query(
        "UPDATE customers SET your_message = 'Kitchen island, quartz', referral_source = 'Google',
                address = '1 Main St' WHERE id = 9100",
    )
    .execute(&pool)
    .await
    .unwrap();
    let res = server(pool)
        .get("/v1/customers/lead-inbox")
        .with_session(sessions::MANAGER)
        .await;
    res.assert_status(StatusCode::OK);
    let inbox: Value = res.json();
    let lead = &inbox["unassigned"][0];
    assert_eq!(lead["your_message"], "Kitchen island, quartz");
    assert_eq!(lead["referral_source"], "Google");
    assert_eq!(lead["address"], "1 Main St");
    // Company 100's live pipelines, oldest first; 100 "Sales" is the default.
    let groups = inbox["groups"].as_array().unwrap();
    let group_ids: Vec<i64> = groups.iter().map(|g| g["id"].as_i64().unwrap()).collect();
    assert!(group_ids.contains(&100) && group_ids.contains(&104));
    let sales = groups.iter().find(|g| g["id"] == 100).unwrap();
    assert_eq!(sales["is_default"], true);
}

#[test_db::test(seed)]
async fn a_rep_gets_no_pipelines(pool: MySqlPool) {
    let res = server(pool)
        .get("/v1/customers/lead-inbox")
        .with_session(sessions::REP)
        .await;
    res.assert_status(StatusCode::OK);
    let inbox: Value = res.json();
    assert_eq!(inbox["groups"], serde_json::json!([]));
}

#[test_db::test(seed)]
async fn assigning_with_a_group_starts_the_deal_in_its_first_stage(pool: MySqlPool) {
    seed_leads(&pool).await;
    let server = server(pool.clone());
    server
        .put("/v1/customers/9100/sales-rep")
        .with_session(sessions::MANAGER)
        .json(&serde_json::json!({ "sales_rep": 100, "group_id": 104 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    // Pipeline 104 "Remodel" starts with stage 104.
    let list: i32 = sqlx::query_scalar(
        "SELECT list_id FROM deals WHERE customer_id = 9100 AND deleted_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(list, 104);
}

#[test_db::test(seed)]
async fn another_companys_group_is_refused(pool: MySqlPool) {
    seed_leads(&pool).await;
    let server = server(pool.clone());
    server
        .put("/v1/customers/9100/sales-rep")
        .with_session(sessions::MANAGER)
        .json(&serde_json::json!({ "sales_rep": 100, "group_id": 101 }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let rep: Option<i32> = sqlx::query_scalar("SELECT sales_rep FROM customers WHERE id = 9100")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rep, None);
}

#[test_db::test(seed)]
async fn each_pipeline_lists_its_live_lists_in_board_order(pool: MySqlPool) {
    let res = server(pool)
        .get("/v1/customers/lead-inbox")
        .with_session(sessions::MANAGER)
        .await;
    res.assert_status(StatusCode::OK);
    let inbox: Value = res.json();
    let groups = inbox["groups"].as_array().unwrap();
    let sales = groups.iter().find(|g| g["id"] == 100).unwrap();
    // 102 "Archived" is deleted.
    assert_eq!(
        sales["lists"],
        serde_json::json!([{ "id": 100, "name": "New" }, { "id": 101, "name": "Quoted" }])
    );
    let remodel = groups.iter().find(|g| g["id"] == 104).unwrap();
    assert_eq!(ids_of(&remodel["lists"]), vec![104, 105]);
}

fn ids_of(list: &Value) -> Vec<i64> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_i64().unwrap())
        .collect()
}

#[test_db::test(seed)]
async fn assigning_with_a_list_starts_the_deal_in_that_list(pool: MySqlPool) {
    seed_leads(&pool).await;
    let server = server(pool.clone());
    // List 105 "Install" (pipeline 104) wins over the group.
    server
        .put("/v1/customers/9100/sales-rep")
        .with_session(sessions::MANAGER)
        .json(&serde_json::json!({ "sales_rep": 100, "group_id": 100, "list_id": 105 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let (list, status): (i32, Option<String>) = sqlx::query_as(
        "SELECT list_id, status FROM deals WHERE customer_id = 9100 AND deleted_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((list, status.as_deref()), (105, Some("Install")));
}

#[test_db::test(seed)]
async fn another_companys_or_a_deleted_list_is_refused(pool: MySqlPool) {
    seed_leads(&pool).await;
    let server = server(pool.clone());
    // 103 is company 101's; 102 "Archived" is deleted.
    for list in [103, 102] {
        server
            .put("/v1/customers/9100/sales-rep")
            .with_session(sessions::MANAGER)
            .json(&serde_json::json!({ "sales_rep": 100, "list_id": list }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    let rep: Option<i32> = sqlx::query_scalar("SELECT sales_rep FROM customers WHERE id = 9100")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rep, None);
}
