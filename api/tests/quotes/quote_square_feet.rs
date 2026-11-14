//! `GET /v1/quotes/square-feet`: many quotes' countertop sq ft in one call.

use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// A straight counter `length` x 25.5 in, in `area`.
fn counter(id: &str, area: &str, length: f64) -> Value {
    json!({
        "id": id, "areaId": area, "x": 0, "y": 0,
        "runs": [{"length": length, "depth": 25.5, "cutouts": []}],
        "edges": [{"type": "unfinished"}, {"type": "finished"}, {"type": "finished"}, {"type": "finished"}],
        "corners": [[null, null], [null, null]],
    })
}

/// Kitchen (x1): 100 x 25.5 in = 17.71 sq ft. Island (x2): 60 x 25.5 in = 10.63 sq ft.
fn drawing() -> Value {
    json!({
        "version": 2,
        "areas": [
            {"id": "a1", "name": "Kitchen", "quantity": 1},
            {"id": "a2", "name": "Island", "quantity": 2},
        ],
        "colorOptions": [{"id": "option-1", "byArea": {}}],
        "items": [],
        "counters": [counter("c1", "a1", 100.0), counter("c2", "a2", 60.0)],
    })
}

fn price_list(rounding: Option<&str>) -> Value {
    let mut list = json!({
        "units": "in", "currency": "$", "expirationDays": 30, "lineItemPriceRounding": 2,
        "arrProducts": [], "arrOtherItems": [], "arrSinkModels": [],
    });
    if let Some(rounding) = rounding {
        list["materialRounding"] = json!(rounding);
    }
    list
}

async fn quote(
    pool: &MySqlPool,
    id: i32,
    company_id: i32,
    drawing: Option<Value>,
    snapshot: Option<Value>,
    revision_id: Option<i32>,
) {
    sqlx::query(
        "INSERT INTO quotes (id, customer_id, quote_name, quote_type, created_date, sales_rep, company_id,
                             salesperson_id, created_by, status, draft_drawing_json, price_snapshot_json,
                             price_list_revision_id)
         VALUES (?, 1000, 'Kitchen', 'quote', NOW(), 'Rita Rep', ?, 100, 100, 'converted', ?, ?, ?)",
    )
    .bind(id)
    .bind(company_id)
    .bind(drawing.map(|d| d.to_string()))
    .bind(snapshot.map(|s| s.to_string()))
    .bind(revision_id)
    .execute(pool)
    .await
    .unwrap();
}

/// A price list of `company_id` whose only revision rounds sq ft `rounding`.
async fn price_list_revision(pool: &MySqlPool, company_id: i32, rounding: &str) -> i32 {
    let list = sqlx::query(
        "INSERT INTO price_lists (company_id, name, current_revision_id) VALUES (?, 'Retail', NULL)",
    )
    .bind(company_id)
    .execute(pool)
    .await
    .unwrap()
    .last_insert_id();
    let revision = sqlx::query(
        "INSERT INTO price_list_revisions (price_list_id, revision_number, info_json) VALUES (?, 1, ?)",
    )
    .bind(list)
    .bind(price_list(Some(rounding)).to_string())
    .execute(pool)
    .await
    .unwrap()
    .last_insert_id();
    i32::try_from(revision).unwrap()
}

async fn square_feet(server: &axum_test::TestServer, ids: &str) -> Value {
    let res = server
        .get(&format!("/v1/quotes/square-feet?{ids}"))
        .with_session(sessions::REP)
        .await;
    res.assert_status_ok();
    res.json::<Value>()["square_feet"].clone()
}

#[test_db::test(seed)]
async fn sums_every_area_rounded_as_its_price_list_rounds(pool: MySqlPool) {
    // To 0.1 sq ft: 17.7 + 2 x 10.6.
    quote(&pool, 9801, 100, Some(drawing()), Some(price_list(None)), None).await;
    // Up to whole feet: 18 + 2 x 11.
    quote(&pool, 9802, 100, Some(drawing()), Some(price_list(Some("up"))), None).await;
    // No snapshot: its price list revision's rounding.
    let up = price_list_revision(&pool, 100, "up").await;
    quote(&pool, 9803, 100, Some(drawing()), None, Some(up)).await;
    // Another company's revision is not this quote's price list.
    let other = price_list_revision(&pool, 101, "up").await;
    quote(&pool, 9804, 100, Some(drawing()), None, Some(other)).await;
    // No drawing yet.
    quote(&pool, 9805, 100, None, None, None).await;
    let server = server(pool);
    assert_eq!(
        square_feet(&server, "ids=9801&ids=9802&ids=9803&ids=9804&ids=9805").await,
        json!({ "9801": 38.9, "9802": 40.0, "9803": 40.0, "9804": 38.9, "9805": 0.0 })
    );
}

#[test_db::test(seed)]
async fn leaves_out_deleted_unknown_and_other_companies_quotes(pool: MySqlPool) {
    quote(&pool, 9811, 100, Some(drawing()), Some(price_list(None)), None).await;
    quote(&pool, 9812, 100, Some(drawing()), Some(price_list(None)), None).await;
    quote(&pool, 9813, 101, Some(drawing()), Some(price_list(None)), None).await;
    sqlx::query("UPDATE quotes SET deleted_at = NOW() WHERE id = 9812")
        .execute(&pool)
        .await
        .unwrap();
    let server = server(pool);
    // A repeated id is answered once.
    assert_eq!(
        square_feet(&server, "ids=9811&ids=9811&ids=9812&ids=9813&ids=99999").await,
        json!({ "9811": 38.9 })
    );
    assert_eq!(square_feet(&server, "ids=9813").await, json!({}));
}

#[test_db::test(seed)]
async fn refuses_no_ids_too_many_and_bad_ones(pool: MySqlPool) {
    let server = server(pool);
    let refused = |query: String| {
        let request = server
            .get(&format!("/v1/quotes/square-feet{query}"))
            .with_session(sessions::REP);
        async move {
            let res = request.await;
            res.assert_status(StatusCode::BAD_REQUEST);
            res.json::<Value>()["error"].as_str().unwrap().to_string()
        }
    };
    assert_eq!(
        refused(String::new()).await,
        "Pass at least one quote id as `ids`."
    );
    let many: Vec<String> = (1..=201).map(|id| format!("ids={id}")).collect();
    assert_eq!(
        refused(format!("?{}", many.join("&"))).await,
        "At most 200 quotes at a time."
    );
    assert!(refused("?ids=abc".into()).await.starts_with("invalid query string"));
    // 200 is fine.
    let most: Vec<String> = (1..=200).map(|id| format!("ids={id}")).collect();
    assert_eq!(square_feet(&server, &most.join("&")).await, json!({}));
}

#[test_db::test(seed)]
async fn needs_an_employee_session(pool: MySqlPool) {
    let server = server(pool);
    server
        .get("/v1/quotes/square-feet?ids=1")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .get("/v1/quotes/square-feet?ids=1")
        .with_session(sessions::VIEWER)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}
