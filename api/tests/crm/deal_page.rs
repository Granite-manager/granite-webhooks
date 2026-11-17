use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::Value;
use sqlx::MySqlPool;

async fn get(pool: &MySqlPool, path: &str) -> Value {
    let res = server(pool.clone()).get(path).with_session(sessions::REP).await;
    res.assert_status_ok();
    res.json()
}

#[test_db::test(seed)]
async fn the_page_matches_the_separate_routes(pool: MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO quotes (id, customer_id, quote_name, quote_type, created_date, sales_rep, company_id, deal_id)
           VALUES (9901, 1000, 'Smith: Kitchen', 'kitchen', NOW(), 'Rita', 100, 8000);
         INSERT INTO quote_emails (id, quote_id, revision_number, token, recipients, subject, sent_by, sent_at)
           VALUES (9911, 9901, 1, 'tok-9911', 'alice@example.com', 'Your quote', 100, '2026-05-01 14:30:00');",
    )
    .execute(&pool)
    .await
    .unwrap();

    let page = get(&pool, "/v1/deals/8000/page").await;
    assert_eq!(page["deal"], get(&pool, "/v1/deals/8000").await);
    for (field, route) in [
        ("stage_history", "stage-history"),
        ("activities", "activities"),
        ("notes", "notes"),
        ("emails", "emails"),
        ("document_events", "document-events"),
    ] {
        let alone = get(&pool, &format!("/v1/deals/8000/{route}")).await;
        assert_eq!(page[field], alone, "{field}");
    }
    assert_eq!(page["document_events"][0]["id"], "quote-email-9911");
}

#[test_db::test(seed)]
async fn another_companys_deal_is_not_found(pool: MySqlPool) {
    server(pool)
        .get("/v1/deals/8000/page")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn needs_a_session(pool: MySqlPool) {
    server(pool)
        .get("/v1/deals/8000/page")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}
