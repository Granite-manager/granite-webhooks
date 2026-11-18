use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: company 100 has pipelines 100 "Sales" (default; stages 100 New,
// 101 Quoted, 102 deleted), 104 "Remodel" (104, 105) and 105 "Hidden"
// (not displayed; 106). Company 101 has 101 (103). The old shared
// pipeline 1 is retired (migration): every company owns its pipelines.

const ADMIN: &str = sessions::ADMIN;

async fn pipelines(server: &axum_test::TestServer) -> Value {
    let res = server.get("/v1/deal-pipelines").with_session(ADMIN).await;
    res.assert_status_ok();
    res.json()
}

fn find(list: &Value, id: i64) -> Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == id)
        .cloned()
        .unwrap_or(Value::Null)
}

#[test_db::test(seed)]
async fn admins_list_every_pipeline_with_its_stages(pool: MySqlPool) {
    let server = server(pool);
    let list = pipelines(&server).await;
    let ids: Vec<i64> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_i64().unwrap())
        .collect();
    assert!(!ids.contains(&1) && ids.contains(&100) && ids.contains(&104) && ids.contains(&105));
    assert!(!ids.contains(&101));
    assert_eq!(
        find(&list, 100),
        json!({
            "id": 100, "name": "Sales", "is_displayed": true, "is_default": true,
            "lists": [{ "id": 100, "name": "New", "position": 0 }, { "id": 101, "name": "Quoted", "position": 1 }],
        })
    );
    assert_eq!(find(&list, 105)["is_displayed"], false);

    for session in [sessions::REP, sessions::DEALER] {
        server
            .get("/v1/deal-pipelines")
            .with_session(session)
            .await
            .assert_status(StatusCode::FORBIDDEN);
    }
}

#[test_db::test(seed)]
async fn pipelines_and_stages_are_created_ordered_and_deleted(pool: MySqlPool) {
    let server = server(pool);
    let res = server
        .post("/v1/deal-pipelines")
        .with_session(ADMIN)
        .json(&json!({ "name": " Commercial " }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let group = res.json::<Value>()["id"].as_i64().unwrap();
    let mut stages = Vec::new();
    for name in ["Lead", "Bid", "Signed"] {
        let res = server
            .post(&format!("/v1/deal-pipelines/{group}/stages"))
            .with_session(ADMIN)
            .json(&json!({ "name": name }))
            .await;
        res.assert_status(StatusCode::CREATED);
        stages.push(res.json::<Value>()["id"].as_i64().unwrap());
    }
    let created = find(&pipelines(&server).await, group);
    assert_eq!(created["name"], "Commercial");
    assert_eq!(
        created["lists"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["name"].clone())
            .collect::<Vec<_>>(),
        ["Lead", "Bid", "Signed"]
    );

    // Not every stage: refused.
    server
        .put(&format!("/v1/deal-pipelines/{group}/stage-order"))
        .with_session(ADMIN)
        .json(&json!({ "list_ids": [stages[2], stages[0]] }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .put(&format!("/v1/deal-pipelines/{group}/stage-order"))
        .with_session(ADMIN)
        .json(&json!({ "list_ids": [stages[2], stages[0], stages[1]] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let ordered = find(&pipelines(&server).await, group);
    assert_eq!(
        ordered["lists"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["name"].clone())
            .collect::<Vec<_>>(),
        ["Signed", "Lead", "Bid"]
    );

    server
        .delete(&format!("/v1/deal-stages/{}", stages[1]))
        .with_session(ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        find(&pipelines(&server).await, group)["lists"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    server
        .delete(&format!("/v1/deal-pipelines/{group}"))
        .with_session(ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(find(&pipelines(&server).await, group), Value::Null);
}

#[test_db::test(seed)]
async fn default_and_display_switch(pool: MySqlPool) {
    let server = server(pool);
    server
        .patch("/v1/deal-pipelines/104")
        .with_session(ADMIN)
        .json(&json!({ "is_default": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let list = pipelines(&server).await;
    assert_eq!(find(&list, 104)["is_default"], true);
    assert_eq!(find(&list, 100)["is_default"], false);
    // Boards open on the first group: the default one.
    assert_eq!(board_groups(&server).await.first(), Some(&104));

    server
        .patch("/v1/deal-pipelines/105")
        .with_session(ADMIN)
        .json(&json!({ "is_displayed": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(find(&pipelines(&server).await, 105)["is_displayed"], true);
}

#[test_db::test(seed)]
async fn the_retired_shared_pipeline_and_other_companies_are_off_limits(pool: MySqlPool) {
    let server = server(pool.clone());
    server
        .delete("/v1/deal-pipelines/1")
        .with_session(ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .post("/v1/deal-pipelines/1/stages")
        .with_session(ADMIN)
        .json(&json!({ "name": "X" }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    // Company 101's pipeline and stage.
    server
        .delete("/v1/deal-pipelines/101")
        .with_session(ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .patch("/v1/deal-pipelines/101")
        .with_session(ADMIN)
        .json(&json!({ "is_displayed": false }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .delete("/v1/deal-stages/103")
        .with_session(ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    let untouched: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT deleted_at FROM deals_list WHERE id = 103")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(untouched.is_none());
    server
        .post("/v1/deal-pipelines")
        .with_session(ADMIN)
        .json(&json!({ "name": "  " }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

// ---- stage names are unique per pipeline (QA audit 2026-10-14, B-034) ----------

#[test_db::test(seed)]
async fn duplicate_stage_names_in_one_pipeline_are_rejected(pool: MySqlPool) {
    let server = server(pool);
    let res = server
        .post("/v1/deal-pipelines")
        .with_session(ADMIN)
        .json(&json!({ "name": "QA Group" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let group = res.json::<Value>()["id"].as_i64().unwrap();
    let add = |name: &str| {
        server
            .post(&format!("/v1/deal-pipelines/{group}/stages"))
            .with_session(ADMIN)
            .json(&json!({ "name": name }))
    };
    add("QA List 1").await.assert_status(StatusCode::CREATED);
    let dup = add("QA List 1").await;
    dup.assert_status(StatusCode::BAD_REQUEST);
    assert!(
        dup.json::<Value>()["error"]
            .as_str()
            .unwrap()
            .contains("already exists"),
        "{}",
        dup.text()
    );
    // The same name in another pipeline is fine.
    let other = server
        .post("/v1/deal-pipelines")
        .with_session(ADMIN)
        .json(&json!({ "name": "QA Group 2" }))
        .await;
    let other = other.json::<Value>()["id"].as_i64().unwrap();
    server
        .post(&format!("/v1/deal-pipelines/{other}/stages"))
        .with_session(ADMIN)
        .json(&json!({ "name": "QA List 1" }))
        .await
        .assert_status(StatusCode::CREATED);
}

/// Ids of the pipelines on the admin's deal board.
async fn board_groups(server: &axum_test::TestServer) -> Vec<i64> {
    let res = server.get("/v1/deal-groups").with_session(ADMIN).await;
    res.assert_status_ok();
    res.json::<Value>()
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["id"].as_i64().unwrap())
        .collect()
}

#[test_db::test(seed)]
async fn any_pipeline_hides_and_deleting_the_default_picks_another(pool: MySqlPool) {
    let server = server(pool.clone());
    assert!(board_groups(&server).await.contains(&100));

    // Even the default pipeline may hide.
    server
        .patch("/v1/deal-pipelines/100")
        .with_session(ADMIN)
        .json(&json!({ "is_displayed": false }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(!board_groups(&server).await.contains(&100));

    // Deleting the default makes the oldest remaining pipeline the default.
    server
        .delete("/v1/deal-pipelines/100")
        .with_session(ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let list = pipelines(&server).await;
    assert_eq!(find(&list, 100), Value::Null);
    assert_eq!(find(&list, 104)["is_default"], true);
    // Only this company: 101 keeps its own default.
    let other: Option<bool> =
        sqlx::query_scalar("SELECT is_default FROM groups_list WHERE id = 101")
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert_eq!(other, Some(true));
}

// ---- "Assign leads here" can be turned off (2026-11-18) -------------------

#[test_db::test(seed)]
async fn the_default_can_be_turned_off_and_stays_off(pool: MySqlPool) {
    let server = server(pool);
    server
        .patch("/v1/deal-pipelines/100")
        .with_session(ADMIN)
        .json(&json!({ "is_default": false }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let list = pipelines(&server).await;
    assert_eq!(find(&list, 100)["is_default"], false);
    assert_eq!(find(&list, 104)["is_default"], false);
    // Deleting another pipeline does not make one the default again.
    server
        .delete("/v1/deal-pipelines/105")
        .with_session(ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let list = pipelines(&server).await;
    assert!(list.as_array().unwrap().iter().all(|p| p["is_default"] == false));
}
