use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: Alice (1000) is company 100's; company 101's customer 2000 is out
// of reach.

#[test_db::test(seed)]
async fn a_customer_lists_its_files_newest_first(pool: MySqlPool) {
    sqlx::query(
        "INSERT INTO customer_files (company_id, customer_id, name, kind, label, file_date, url, source_id)
         VALUES (100, 1000, 'Quote_Signature_08_14_2026', 'signed_contract', 'Quote #11934', '2026-08-14', 'https://files.test/a.pdf', 'q1'),
                (100, 1000, 'Invoice #11162', 'invoice', 'Invoice #11162', '2026-09-02', 'https://files.test/b.pdf', 'i1'),
                (101, 2000, 'Other', 'upload', NULL, NULL, 'https://files.test/c.pdf', 'o1')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool);
    let res = server
        .get("/v1/customers/1000/files")
        .with_session(sessions::REP)
        .await;
    res.assert_status_ok();
    let body: Value = res.json();
    assert_eq!(
        body,
        json!([
            { "id": body[0]["id"], "name": "Invoice #11162", "kind": "invoice", "label": "Invoice #11162",
              "file_date": "2026-09-02", "url": "https://files.test/b.pdf" },
            { "id": body[1]["id"], "name": "Quote_Signature_08_14_2026", "kind": "signed_contract",
              "label": "Quote #11934", "file_date": "2026-08-14", "url": "https://files.test/a.pdf" },
        ])
    );
    server
        .get("/v1/customers/2000/files")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}
