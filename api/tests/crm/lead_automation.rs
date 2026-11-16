use crate::common;

use axum::http::StatusCode;
use axum_test::TestServer;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Employee CRM › Automations. Seed: company 100's template 8670 emails, and
// 8671 texts, deals entering list 101. Dina (107) and Rita (100) are reps of
// company 100. CloudTalk is turned on here (texting needs a provider).

const DINA: &str = sessions::DEALER;
const RITA: &str = sessions::REP;

async fn with_cloudtalk(pool: &MySqlPool) {
    sqlx::query(
        "UPDATE company SET cloudtalk_access_key = 'k', cloudtalk_access_secret = 's' WHERE id = 100",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn scalar_i64(pool: &MySqlPool, sql: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql.to_string()))
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn mode(server: &TestServer, session: &str) -> Value {
    let res = server
        .get("/v1/me/lead-automation")
        .with_session(session)
        .await;
    res.assert_status_ok();
    res.json::<Value>()["mode"].clone()
}

async fn set_mode(server: &TestServer, session: &str, mode: &str) {
    let res = server
        .put("/v1/me/lead-automation")
        .with_session(session)
        .json(&json!({ "mode": mode }))
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["mode"], mode);
}

/// Dina's own email drip on list 101, right away; returns its id.
async fn own_email_drip(server: &TestServer, session: &str, hours: i64) -> i64 {
    let res = server
        .post("/v1/email-templates")
        .with_session(session)
        .json(&json!({
            "template_name": "My hello",
            "template_subject": "Hello",
            "template_body": "<p>Hi {{customer.first_name}}</p>",
            "lead_list_id": 101,
            "hour_delay": hours,
            "sales_rep_ids": [100],
        }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let created: Value = res.json();
    assert_eq!(created["scope"], "personal");
    assert_eq!(created["lead_list_id"], 101);
    assert_eq!(created["hour_delay"], hours);
    // A personal drip goes to its owner's deals: no reps to pick.
    assert_eq!(created["sales_rep_ids"], json!([]));
    created["id"].as_i64().unwrap()
}

async fn own_sms_drip(server: &TestServer, session: &str) -> i64 {
    let res = server
        .post("/v1/sms-templates")
        .with_session(session)
        .json(&json!({
            "name": "My text",
            "body": "Hi {{customer.first_name}}",
            "drip": { "lead_list_id": 101, "hour_delay": 0, "minute_delay": 5 },
        }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let created: Value = res.json();
    assert_eq!(created["scope"], "personal");
    assert_eq!(created["lead_list_id"], 101);
    assert_eq!(created["minute_delay"], 5);
    created["id"].as_i64().unwrap()
}

/// A new lead of Dina's lands on list 101; returns the deal id.
async fn new_lead_deal(pool: &MySqlPool, customer_id: i64) -> i64 {
    sqlx::query(
        "INSERT INTO customers (id, name, company_id, source, sales_rep) VALUES (?, 'New Lead', 100, 'Leads', 107)",
    )
    .bind(customer_id)
    .execute(pool)
    .await
    .unwrap();
    let res = server(pool.clone())
        .post("/v1/deals")
        .with_session(DINA)
        .json(&json!({ "customer_id": customer_id, "list_id": 101 }))
        .await;
    res.assert_status(StatusCode::CREATED);
    res.json::<Value>()["id"].as_i64().unwrap()
}

async fn pending(pool: &MySqlPool, table: &str, deal_id: i64, template_id: i64) -> i64 {
    scalar_i64(
        pool,
        &format!(
            "SELECT COUNT(*) FROM {table} WHERE deal_id = {deal_id} AND template_id = {template_id} AND status = 'pending'"
        ),
    )
    .await
}

#[test_db::test(seed)]
async fn reps_get_the_company_automations_or_their_own(pool: MySqlPool) {
    with_cloudtalk(&pool).await;
    let server = server(pool.clone());
    assert_eq!(mode(&server, DINA).await, "company");
    let own_email = own_email_drip(&server, DINA, 0).await;
    let own_sms = own_sms_drip(&server, DINA).await;

    // Company settings: the company's drips, not Dina's own.
    let first = new_lead_deal(&pool, 8690).await;
    assert_eq!(pending(&pool, "scheduled_emails", first, 8670).await, 1);
    assert_eq!(pending(&pool, "scheduled_sms", first, 8671).await, 1);
    assert_eq!(
        pending(&pool, "scheduled_emails", first, own_email).await,
        0
    );
    assert_eq!(pending(&pool, "scheduled_sms", first, own_sms).await, 0);

    // Her own flow: the company's pending sends on her deals stop...
    set_mode(&server, DINA, "own").await;
    assert_eq!(mode(&server, DINA).await, "own");
    assert_eq!(pending(&pool, "scheduled_emails", first, 8670).await, 0);
    assert_eq!(pending(&pool, "scheduled_sms", first, 8671).await, 0);
    // ...and nothing is backfilled for deals already on the list.
    assert_eq!(
        pending(&pool, "scheduled_emails", first, own_email).await,
        0
    );

    // Her next lead gets only her own drips.
    let second = new_lead_deal(&pool, 8691).await;
    assert_eq!(
        pending(&pool, "scheduled_emails", second, own_email).await,
        1
    );
    assert_eq!(pending(&pool, "scheduled_sms", second, own_sms).await, 1);
    assert_eq!(pending(&pool, "scheduled_emails", second, 8670).await, 0);
    assert_eq!(pending(&pool, "scheduled_sms", second, 8671).await, 0);

    // Rita still uses the company's.
    assert_eq!(mode(&server, RITA).await, "company");

    // Back to company settings: her own pending sends stop.
    set_mode(&server, DINA, "company").await;
    assert_eq!(
        pending(&pool, "scheduled_emails", second, own_email).await,
        0
    );
    assert_eq!(pending(&pool, "scheduled_sms", second, own_sms).await, 0);
    let third = new_lead_deal(&pool, 8692).await;
    assert_eq!(pending(&pool, "scheduled_emails", third, 8670).await, 1);
    assert_eq!(
        pending(&pool, "scheduled_emails", third, own_email).await,
        0
    );

    let res = server
        .put("/v1/me/lead-automation")
        .with_session(DINA)
        .json(&json!({ "mode": "everyone" }))
        .await;
    assert!(res.status_code().is_client_error());
    assert_eq!(mode(&server, DINA).await, "company");
}

#[test_db::test(seed)]
async fn company_drips_go_only_to_the_reps_they_name(pool: MySqlPool) {
    // 8670 is for Rita's deals only: Dina's new lead gets no email.
    sqlx::query("UPDATE email_templates SET sales_rep_ids = JSON_ARRAY(100) WHERE id = 8670")
        .execute(&pool)
        .await
        .unwrap();
    let deal = new_lead_deal(&pool, 8690).await;
    assert_eq!(pending(&pool, "scheduled_emails", deal, 8670).await, 0);
}

#[test_db::test(seed)]
async fn own_drips_are_checked_per_owner(pool: MySqlPool) {
    with_cloudtalk(&pool).await;
    let server = server(pool.clone());
    let own = own_email_drip(&server, DINA, 3).await;

    // Another of Dina's on the same list and delay is refused...
    let res = server
        .post("/v1/email-templates")
        .with_session(DINA)
        .json(&json!({
            "template_name": "Twin",
            "template_subject": "Hello",
            "template_body": "<p>Hi</p>",
            "lead_list_id": 101,
            "hour_delay": 3,
        }))
        .await;
    res.assert_status(StatusCode::CONFLICT);
    assert_eq!(
        res.json::<Value>()["error"],
        "Another of your email templates already uses this list and delay."
    );
    // ...Rita's is not, nor one on the company's delay (8670: 2 hours).
    own_email_drip(&server, RITA, 3).await;
    own_email_drip(&server, DINA, 2).await;

    own_sms_drip(&server, DINA).await;
    let res = server
        .post("/v1/sms-templates")
        .with_session(DINA)
        .json(&json!({
            "name": "Twin",
            "body": "Hi",
            "drip": { "lead_list_id": 101, "hour_delay": 0, "minute_delay": 5 },
        }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        res.json::<Value>()["error"],
        "Another of your SMS templates already uses this list and delay."
    );

    // Removing the automation of her own template calls off its sends.
    set_mode(&server, DINA, "own").await;
    let deal = new_lead_deal(&pool, 8690).await;
    assert_eq!(pending(&pool, "scheduled_emails", deal, own).await, 1);
    server
        .put(&format!("/v1/email-templates/{own}"))
        .with_session(DINA)
        .json(&json!({
            "template_name": "My hello",
            "template_subject": "Hello",
            "template_body": "<p>Hi</p>",
            "lead_list_id": null,
            "hour_delay": null,
        }))
        .await
        .assert_status_ok();
    assert_eq!(pending(&pool, "scheduled_emails", deal, own).await, 0);
}

/// `(channel, template_id)` of one side of `GET /v1/me/lead-automation/items`.
fn ids(items: &Value, side: &str) -> Vec<(String, i64)> {
    let mut ids: Vec<(String, i64)> = items[side]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["channel"].as_str().unwrap().to_string(),
                item["template_id"].as_i64().unwrap(),
            )
        })
        .collect();
    ids.sort();
    ids
}

#[test_db::test(seed)]
async fn the_items_list_what_goes_to_my_deals_on_each_side(pool: MySqlPool) {
    // Seed: company drips 8670 (email, list 101, 2 h) and 8671 (text, 1 h).
    sqlx::raw_sql(
        "INSERT INTO email_templates (id, template_name, template_subject, template_body, company_id,
                                      user_id, lead_list_id, hour_delay, sales_rep_ids, deleted_at) VALUES
           (9100, 'For Rita',        's', 'b', 100, NULL, 100, NULL, '[100]', NULL),
           (9101, 'For someone else','s', 'b', 100, NULL, 100, 1,    '[101]', NULL),
           (9102, 'Dina own',        's', 'b', 100, 107,  101, 3,    NULL,    NULL),
           (9103, 'Rita own',        's', 'b', 100, 100,  101, 1,    NULL,    NULL),
           (9104, 'No list',         's', 'b', 100, NULL, NULL, 1,   NULL,    NULL),
           (9105, 'Deleted',         's', 'b', 100, NULL, 100, 1,    NULL,    '2026-01-01 00:00:00'),
           (9106, 'On a gone list',  's', 'b', 100, 107,  102, 0,    NULL,    NULL);
         INSERT INTO cloudtalk_sms_templates (id, company_id, user_id, name, body, lead_list_id,
                                              hour_delay, minute_delay, position) VALUES
           (9110, 100, 107, 'Dina own text', 'Hi', 100, 1, 30, 1);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool.clone());

    // No texting provider yet: texts are left out.
    let items: Value = server
        .get("/v1/me/lead-automation/items")
        .with_session(DINA)
        .await
        .json();
    assert_eq!(ids(&items, "company"), vec![("email".into(), 8670)]);
    assert_eq!(
        ids(&items, "own"),
        vec![("email".into(), 9102), ("email".into(), 9106)]
    );

    with_cloudtalk(&pool).await;
    let items: Value = server
        .get("/v1/me/lead-automation/items")
        .with_session(DINA)
        .await
        .json();
    assert_eq!(
        ids(&items, "company"),
        vec![("email".into(), 8670), ("sms".into(), 8671)]
    );
    assert_eq!(
        ids(&items, "own"),
        vec![
            ("email".into(), 9102),
            ("email".into(), 9106),
            ("sms".into(), 9110)
        ]
    );
    let own_text = items["own"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["template_id"] == 9110)
        .unwrap();
    assert_eq!(
        own_text,
        &json!({
            "channel": "sms", "template_id": 9110, "name": "Dina own text",
            "list_id": 100, "list_name": "New", "group_name": "Sales", "delay_minutes": 90,
        })
    );
    let gone = items["own"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["template_id"] == 9106)
        .unwrap();
    assert!(gone["list_name"].is_null() && gone["group_name"].is_null());

    // Rita also gets the company template naming her, and only her own.
    let items: Value = server
        .get("/v1/me/lead-automation/items")
        .with_session(RITA)
        .await
        .json();
    assert_eq!(
        ids(&items, "company"),
        vec![
            ("email".into(), 8670),
            ("email".into(), 9100),
            ("sms".into(), 8671)
        ]
    );
    assert_eq!(ids(&items, "own"), vec![("email".into(), 9103)]);
    let quoted = items["company"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["template_id"] == 8670)
        .unwrap();
    assert_eq!(quoted["delay_minutes"], 120);
    assert_eq!(quoted["list_name"], "Quoted");

    server
        .get("/v1/me/lead-automation/items")
        .await
        .assert_status_unauthorized();
}
