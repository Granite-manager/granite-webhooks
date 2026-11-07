use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

#[test_db::test(seed)]
async fn admins_manage_tax_rates_and_employees_read_them(pool: MySqlPool) {
    let server = server(pool);
    server
        .post("/v1/quote-tax-rates")
        .with_session(sessions::REP)
        .json(&json!({ "name": "County", "tax_pct": 7 }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .post("/v1/quote-tax-rates")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "County", "tax_pct": 101 }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let res = server
        .post("/v1/quote-tax-rates")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": " County ", "sequence": 2, "tax_pct": 7.25 }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let id = res.json::<Value>()["id"].as_i64().unwrap();
    server
        .post("/v1/quote-tax-rates")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "State", "tax_pct": 7 }))
        .await
        .assert_status(StatusCode::CREATED);
    server
        .put(&format!("/v1/quote-tax-rates/{id}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Marion", "sequence": 2, "tax_pct": 7.5 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let list: Value = server
        .get("/v1/quote-tax-rates")
        .with_session(sessions::REP)
        .await
        .json();
    let names: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["State", "Marion"]);
    assert_eq!(list[1]["tax_pct"], 7.5);
    server
        .delete(&format!("/v1/quote-tax-rates/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .delete(&format!("/v1/quote-tax-rates/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    let other: Value = server
        .get("/v1/quote-tax-rates")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json();
    assert_eq!(other, json!([]));
}

#[test_db::test(seed)]
async fn admins_manage_payment_terms(pool: MySqlPool) {
    let server = server(pool);
    server
        .post("/v1/quote-payment-terms")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Deposit", "terms": "  " }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let res = server
        .post("/v1/quote-payment-terms")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Deposit", "terms": "50% now" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let id = res.json::<Value>()["id"].as_i64().unwrap();
    server
        .put(&format!("/v1/quote-payment-terms/{id}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Deposit", "terms": "60% now" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let list: Value = server
        .get("/v1/quote-payment-terms")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        list,
        json!([{ "id": id, "name": "Deposit", "sequence": 1, "terms": "60% now" }])
    );
}

#[test_db::test(seed)]
async fn quote_settings_default_then_save(pool: MySqlPool) {
    let server = server(pool);
    let settings: Value = server
        .get("/v1/quote-settings")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        settings,
        json!({ "units": "in", "rounding": 0.0625, "default_depth": 25.5 })
    );
    server
        .put("/v1/quote-settings")
        .with_session(sessions::ADMIN)
        .json(&json!({ "rounding": 0.25, "default_depth": 0 }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .put("/v1/quote-settings")
        .with_session(sessions::ADMIN)
        .json(&json!({ "rounding": 0.25, "default_depth": 26.123 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let settings: Value = server
        .get("/v1/quote-settings")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(settings["rounding"], 0.25);
    assert_eq!(settings["default_depth"], 26.12);
}

#[test_db::test(seed)]
async fn company_quote_email_templates_stay_numbered(pool: MySqlPool) {
    let server = server(pool);
    for name in ["First", "Second"] {
        server
            .post("/v1/quote-email-templates")
            .with_session(sessions::ADMIN)
            .json(&json!({ "name": name, "subject": "Your quote", "body": "Hi", "sequence": 9 }))
            .await
            .assert_status(StatusCode::CREATED);
    }
    let list: Value = server
        .get("/v1/quote-email-templates")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(list[0]["name"], "First");
    assert_eq!(list[0]["sequence"], 1);
    assert_eq!(list[1]["sequence"], 2);
    assert_eq!(list[0]["body"], "Hi\n\n[PDFLink]");
    let second = list[1]["id"].as_i64().unwrap();
    server
        .put(&format!("/v1/quote-email-templates/{second}"))
        .with_session(sessions::ADMIN)
        .json(
            &json!({ "name": "Second", "subject": "Quote", "body": "[QuoteLink]", "sequence": 1 }),
        )
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let list: Value = server
        .get("/v1/quote-email-templates")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(list[0]["name"], "Second");
    assert_eq!(list[0]["body"], "[QuoteLink]");
    server
        .delete(&format!("/v1/quote-email-templates/{second}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let list: Value = server
        .get("/v1/quote-email-templates")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["sequence"], 1);
}

#[test_db::test(seed)]
async fn online_quote_options_are_the_companys_extras(pool: MySqlPool) {
    let server = server(pool.clone());
    server
        .get("/v1/online-quote-options")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    // None to start.
    let list: Value = server
        .get("/v1/online-quote-options")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(list, json!([]));
    sqlx::raw_sql(
        "INSERT INTO quote_extras (company_id, sequence, item_json) VALUES
           (100, 1, '{\"name\":\"Stone/Tile Tear Out\"}'),
           (100, 2, '{\"name\":\"10 year Sealer\"}'),
           (100, 3, '{\"name\":\"Plumbing\"}'),
           (101, 1, '{\"name\":\"Elsewhere\"}')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let extra = |name: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i32>(
                "SELECT id FROM quote_extras WHERE JSON_UNQUOTE(JSON_EXTRACT(item_json, '$.name')) = ?",
            )
            .bind(name)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let tear_out = extra("Stone/Tile Tear Out").await;
    let sealer = extra("10 year Sealer").await;
    let elsewhere = extra("Elsewhere").await;

    let add = |body: Value| {
        server
            .post("/v1/online-quote-options")
            .with_session(sessions::ADMIN)
            .json(&body)
    };
    add(json!({ "extra_id": tear_out, "sequence": 1 }))
        .await
        .assert_status(StatusCode::CREATED);
    add(json!({ "extra_id": sealer, "sequence": 2, "enabled": true }))
        .await
        .assert_status(StatusCode::CREATED);
    // By name (an older page), any case.
    add(json!({ "name": " plumbing ", "sequence": 3 }))
        .await
        .assert_status(StatusCode::CREATED);
    // Only the company's own extras, each once.
    add(json!({ "extra_id": elsewhere }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    add(json!({ "name": "Haul away" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    add(json!({ "extra_id": sealer }))
        .await
        .assert_status(StatusCode::CONFLICT);

    let list: Value = server
        .get("/v1/online-quote-options")
        .with_session(sessions::ADMIN)
        .await
        .json();
    let first = list[0]["id"].as_i64().unwrap();
    server
        .put(&format!("/v1/online-quote-options/{first}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "extra_id": tear_out, "sequence": 1, "enabled": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    // A renamed extra shows its new name; a deleted one drops out.
    sqlx::raw_sql(
        "UPDATE quote_extras SET item_json = '{\"name\":\"Stone Tear Out\"}' WHERE JSON_UNQUOTE(JSON_EXTRACT(item_json, '$.name')) = 'Stone/Tile Tear Out';
         UPDATE quote_extras SET deleted_at = NOW() WHERE JSON_UNQUOTE(JSON_EXTRACT(item_json, '$.name')) = 'Plumbing'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let list: Value = server
        .get("/v1/online-quote-options")
        .with_session(sessions::ADMIN)
        .await
        .json();
    let rows: Vec<(&str, i64, bool)> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["name"].as_str().unwrap(),
                row["extra_id"].as_i64().unwrap(),
                row["enabled"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("Stone Tear Out", i64::from(tear_out), true),
            ("10 year Sealer", i64::from(sealer), true),
        ]
    );
}
