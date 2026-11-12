//! The `crm_text` push for a new `RingCentral` text (`POST /v1/sms/inbound-push`,
//! called by the `webhooks` Lambda with its key), and the notice the phone
//! reads (`GET /v1/sms/mine/messages/{provider}/{id}`).
//!
//! Rita (100, phone 1) has `RingCentral` extension 540273; Mark (101, phone 2)
//! has none. Both are of Acme (100).

use crate::phone_common;

use axum::http::StatusCode;
use axum_test::TestServer;
use phone_common::{JOB_KEY, Stubs, add_phone, phone_server, sql};
use serde_json::{Value, json};
use sqlx::MySqlPool;

async fn setup(pool: &MySqlPool) -> (TestServer, Stubs) {
    let (server, stubs) = phone_server(pool.clone()).await;
    add_phone(pool, 1, 100, 100).await;
    add_phone(pool, 2, 101, 100).await;
    sql(
        pool,
        "UPDATE users SET ringcentral_extension_id = '540273' WHERE id = 100",
    )
    .await;
    // Inbound to Rita's extension; an outbound one; another company's.
    sql(
        pool,
        "INSERT INTO ringcentral_sms
           (id, ringcentral_id, sender, recipient, text, agent, company_id, direction, status,
            sender10, recipient10, phone_digits, is_echo)
         VALUES
           (97001, 1, 3175550100, 3173161456, 'Is the slab still available?', '540273', 100,
            'inbound', 'received', '3175550100', '3173161456', '3175550100', 0),
           (97002, 2, 3173161456, 3175550100, 'Yes', '540273', 100,
            'outbound', 'sent', '3173161456', '3175550100', '3175550100', 0),
           (97003, 3, 3175550100, 3173161456, 'Hi', '540273', 101,
            'inbound', 'received', '3175550100', '3173161456', '3175550100', 0)",
    )
    .await;
    (server, stubs)
}

async fn push(server: &TestServer, provider: &str, sms_id: i64) -> Value {
    let res = server
        .post("/v1/sms/inbound-push")
        .add_header("authorization", JOB_KEY)
        .json(&json!({ "company_id": 100, "provider": provider, "sms_id": sms_id }))
        .await;
    res.assert_status_ok();
    res.json()
}

fn text_pushes(stubs: &Stubs) -> Vec<(String, Value)> {
    stubs
        .pushes()
        .into_iter()
        .filter(|push| push["data"]["type"] == "crm_text")
        .map(|push| {
            (
                push["token"].as_str().unwrap().to_string(),
                push["data"].clone(),
            )
        })
        .collect()
}

#[test_db::test(seed)]
async fn a_new_text_pushes_the_phone_of_whose_inbox_shows_it(pool: MySqlPool) {
    let (server, stubs) = setup(&pool).await;
    assert_eq!(
        push(&server, "ringcentral", 97001).await,
        json!({ "pushed": 1 })
    );
    // Rita's phone only, ids only.
    assert_eq!(
        text_pushes(&stubs),
        [(
            "push-1".to_string(),
            json!({ "granite": "1", "type": "crm_text", "provider": "ringcentral", "smsId": "97001" })
        )]
    );
    // Outbound, another company's, unknown: nothing.
    for id in [97002, 97003, 999_999] {
        assert_eq!(
            push(&server, "ringcentral", id).await,
            json!({ "pushed": 0 }),
            "{id}"
        );
    }
    assert_eq!(
        push(&server, "cloudtalk", 97001).await,
        json!({ "pushed": 0 })
    );
    assert_eq!(text_pushes(&stubs).len(), 1);
}

#[test_db::test(seed)]
async fn the_push_needs_the_key_and_a_crm_provider(pool: MySqlPool) {
    let (server, stubs) = setup(&pool).await;
    server
        .post("/v1/sms/inbound-push")
        .json(&json!({ "company_id": 100, "provider": "ringcentral", "sms_id": 97001 }))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    for provider in ["telnyx", "fax"] {
        server
            .post("/v1/sms/inbound-push")
            .add_header("authorization", JOB_KEY)
            .json(&json!({ "company_id": 100, "provider": provider, "sms_id": 97001 }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    assert!(text_pushes(&stubs).is_empty());
}

#[test_db::test(seed)]
async fn the_notice_is_only_for_whose_inbox_shows_it(pool: MySqlPool) {
    let (server, _) = setup(&pool).await;
    let notice: Value = server
        .get("/v1/sms/mine/messages/ringcentral/97001")
        .add_header("x-granite-device", phone_common::token(1))
        .await
        .json();
    assert_eq!(notice["phone"], "3175550100");
    assert_eq!(notice["text"], "Is the slab still available?");
    assert_eq!(notice["has_media"], false);
    // Mark's inbox doesn't have it; nor is an outbound text a notice.
    server
        .get("/v1/sms/mine/messages/ringcentral/97001")
        .add_header("x-granite-device", phone_common::token(2))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .get("/v1/sms/mine/messages/ringcentral/97002")
        .add_header("x-granite-device", phone_common::token(1))
        .await
        .assert_status(StatusCode::NOT_FOUND);
}
