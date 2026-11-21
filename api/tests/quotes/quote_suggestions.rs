use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// A quote of company 100 for `customer` with these area and misc names.
async fn quote(pool: &MySqlPool, id: i32, customer: i32, areas: &[&str], misc: &[&str]) {
    let doc = json!({
        "areas": areas.iter().map(|name| json!({ "name": name })).collect::<Vec<_>>(),
        "items": misc
            .iter()
            .map(|name| json!({ "kind": "misc", "name": name, "price": 25 }))
            .collect::<Vec<_>>(),
    });
    sqlx::query(
        "INSERT INTO quotes (id, customer_id, quote_name, quote_type, created_date, sales_rep, company_id,
                             salesperson_id, created_by, status, draft_drawing_json, updated_at)
         VALUES (?, ?, 'Q', 'quote', NOW(), 'Rita Rep', 100, 100, 100, 'draft', ?, NOW() - INTERVAL 1 DAY)",
    )
    .bind(id)
    .bind(customer)
    .bind(doc.to_string())
    .execute(pool)
    .await
    .unwrap();
}

fn names(body: &Value) -> Vec<&str> {
    body["area_names"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["name"].as_str().unwrap())
        .collect()
}

fn items(body: &Value) -> Vec<&str> {
    body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["description"].as_str().unwrap())
        .collect()
}

#[test_db::test(seed)]
async fn names_of_one_customer_are_not_offered_and_the_x_hides_for_the_company(pool: MySqlPool) {
    // Alice's two quotes (a copy) use "Unit 6211"; Kitchen and Trip fee are
    // on quotes of Alice and Erin.
    quote(&pool, 9801, 1000, &["kitchen", "Unit 6211"], &["Trip fee"]).await;
    quote(&pool, 9802, 1000, &["Kitchen", "Unit 6211"], &[]).await;
    quote(&pool, 9803, 1004, &["Kitchen", "Island"], &["Trip fee", "ery"]).await;
    let server = server(pool.clone());

    let body: Value = server
        .get("/v1/quotes/suggestions")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(names(&body), ["Kitchen"]);
    assert_eq!(body["area_names"][0]["uses"], 3);
    assert_eq!(body["area_names"][0]["customers"], 2);
    assert_eq!(items(&body), ["Trip fee"]);
    assert_eq!(body["hidden"], json!([]));

    for bad in [
        json!({ "kind": "deal", "text": "Kitchen" }),
        json!({ "kind": "area", "text": "   " }),
    ] {
        server
            .post("/v1/quotes/suggestions/hide")
            .with_session(sessions::REP)
            .json(&bad)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    for (kind, text) in [("area", " KITCHEN "), ("misc", "Trip  fee"), ("area", "Vanity")] {
        server
            .post("/v1/quotes/suggestions/hide")
            .with_session(sessions::REP)
            .json(&json!({ "kind": kind, "text": text }))
            .await
            .assert_status(StatusCode::NO_CONTENT);
    }
    let stored: (String, String, i32) = sqlx::query_as(
        "SELECT text_key, text, hidden_by FROM quote_suggestion_hidden
          WHERE company_id = 100 AND kind = 'area' AND text_key = 'kitchen'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored, ("kitchen".into(), "KITCHEN".into(), 100));

    // Hidden for everyone in the company, listed for the editor's own names.
    let body: Value = server
        .get("/v1/quotes/suggestions")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(names(&body), Vec::<&str>::new());
    assert_eq!(items(&body), Vec::<&str>::new());
    assert_eq!(
        body["hidden"],
        json!([
            { "kind": "area", "text": "KITCHEN" },
            { "kind": "area", "text": "Vanity" },
            { "kind": "misc", "text": "Trip fee" },
        ])
    );

    // Another company is not affected.
    let other: Value = server
        .get("/v1/quotes/suggestions")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json();
    assert_eq!(other["hidden"], json!([]));

    // Two customers' quotes changed after hiding bring Kitchen back.
    sqlx::query(
        "UPDATE quote_suggestion_hidden SET hidden_at = NOW() - INTERVAL 2 DAY
          WHERE company_id = 100 AND text_key = 'kitchen'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let body: Value = server
        .get("/v1/quotes/suggestions")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(names(&body), ["Kitchen"]);
    assert_eq!(
        body["hidden"],
        json!([
            { "kind": "area", "text": "Vanity" },
            { "kind": "misc", "text": "Trip fee" },
        ])
    );
}


#[test_db::test(seed)]
async fn quotes_with_a_deal_are_read(pool: MySqlPool) {
    // `quotes.deal_id` is BIGINT UNSIGNED: a quote tied to a deal once broke
    // the whole list (500).
    quote(&pool, 9811, 1000, &["Kitchen"], &[]).await;
    quote(&pool, 9812, 1004, &["Kitchen"], &[]).await;
    let deal: u64 = sqlx::query_scalar(
        "SELECT d.id FROM deals d JOIN customers c ON c.id = d.customer_id
          WHERE c.company_id = 100 ORDER BY d.id LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE quotes SET deal_id = ? WHERE id IN (9811, 9812)")
        .bind(deal)
        .execute(&pool)
        .await
        .unwrap();
    let res = server(pool)
        .get("/v1/quotes/suggestions")
        .with_session(sessions::REP)
        .await;
    res.assert_status_ok();
    assert_eq!(names(&res.json()), ["Kitchen"]);
}
