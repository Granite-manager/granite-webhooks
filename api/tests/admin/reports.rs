use crate::common;

use common::{WithSession, server, sessions};
use serde_json::Value;
use sqlx::MySqlPool;

#[test_db::test(seed)]
async fn cut_slabs_report_lists_whole_cut_slabs(pool: MySqlPool) {
    sqlx::query(
        "INSERT INTO slab_inventory (id, stone_id, bundle, sale_id, parent_id, cut_date)
         VALUES (9200, 3000, 'R-1', 7010, NULL, '2026-04-03 10:00:00'),
                (9201, 3000, 'R-1a', NULL, 9200, '2026-04-04 10:00:00'),
                (9202, 3001, 'R-2', NULL, NULL, '2026-05-01 10:00:00')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool);
    let ids = |body: Value| {
        body.as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_i64().unwrap())
            .filter(|id| *id >= 9200)
            .collect::<Vec<_>>()
    };
    let all: Value = server
        .get("/v1/reports/cut-slabs")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(ids(all.clone()), [9202, 9200]);
    let first = all
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == 9200)
        .unwrap();
    assert_eq!(first["stone_name"], "Absolute Black");
    assert_eq!(first["seller_name"], "Rita Rep");
    let april: Value = server
        .get("/v1/reports/cut-slabs?from=2026-04-01&to=2026-04-30&stone_id=3000")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(ids(april), [9200]);
    server
        .get("/v1/reports/cut-slabs")
        .with_session(sessions::REP)
        .await
        .assert_status(axum::http::StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn slabs_report_by_status(pool: MySqlPool) {
    sqlx::query(
        "UPDATE slab_inventory SET hold_until = UTC_TIMESTAMP() + INTERVAL 3 DAY,
                hold_by = (SELECT seller_id FROM sales WHERE id = 7010), hold_at = UTC_TIMESTAMP()
          WHERE id = 6074",
    )
    .execute(&pool)
    .await
    .unwrap();
    // An ended hold counts as available.
    sqlx::query(
        "UPDATE slab_inventory SET hold_until = UTC_TIMESTAMP() - INTERVAL 1 DAY WHERE id = 6070",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool);
    let report = |status: &'static str| {
        let server = &server;
        async move {
            let body: Value = server
                .get(&format!("/v1/reports/slabs?status={status}&stone_id=3010"))
                .with_session(sessions::ADMIN)
                .await
                .json();
            body.as_array().unwrap().clone()
        }
    };
    let ids = |rows: &[Value]| {
        let mut ids: Vec<i64> = rows.iter().map(|row| row["id"].as_i64().unwrap()).collect();
        ids.sort();
        ids
    };
    let on_hold = report("on_hold").await;
    assert_eq!(ids(&on_hold), [6074]);
    assert_eq!(on_hold[0]["held_by_name"], "Rita Rep");
    let available = report("available").await;
    assert!(ids(&available).contains(&6070));
    assert!(!ids(&available).contains(&6074));
    assert!(!ids(&available).contains(&6071));
    let sold = report("sold").await;
    assert!(ids(&sold).contains(&6071));
    assert!(ids(&sold).contains(&6073));
    assert_eq!(
        sold.iter().find(|row| row["id"] == 6071).unwrap()["customer_name"],
        "Alice Lead"
    );
    server
        .get("/v1/reports/slabs?status=available")
        .with_session(sessions::REP)
        .await
        .assert_status(axum::http::StatusCode::FORBIDDEN);
    server
        .get("/v1/reports/slabs?status=nope")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(axum::http::StatusCode::BAD_REQUEST);
}
