//! Pay for templates and installs: rules, the sync, extras and payroll.

use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// A template (finished yesterday, by the manager, a Template technician)
/// and an install (finished yesterday, by the rep, who is no Installer).
async fn finished_jobs(pool: &MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO calendars (id, company_id, name, color, slug) VALUES
           (8840, 100, 'Templates', 'green', 'templates'),
           (8841, 100, 'Installations', 'blue', 'installations');
         INSERT INTO users_positions (user_id, position_id, company_id) VALUES (101, 11, 100);
         INSERT INTO events (id, title, start_date, end_date, created_user_id, assigned_user_id, calendar_id)
           VALUES
           (88401, 'Template: Smith', NOW() - INTERVAL 1 DAY, NOW() - INTERVAL 23 HOUR, 100, 101, 8840),
           (88402, 'Install: Smith', NOW() - INTERVAL 1 DAY, NOW() - INTERVAL 23 HOUR, 100, 100, 8841),
           (88403, 'Template: next week', NOW() + INTERVAL 7 DAY, NOW() + INTERVAL 8 DAY, 100, 101, 8840);",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn sync(server: &axum_test::TestServer) -> Value {
    let res = server
        .post("/v1/commissions/sync")
        .with_session(sessions::ADMIN)
        .await;
    res.assert_status_ok();
    res.json::<Value>()
}

async fn jobs(server: &axum_test::TestServer) -> Value {
    server
        .get("/v1/job-pay")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>()
}

async fn add_rule(server: &axum_test::TestServer, rule: Value) -> i64 {
    let res = server
        .post("/v1/job-pay-rules")
        .with_session(sessions::ADMIN)
        .json(&rule)
        .await;
    res.assert_status(StatusCode::CREATED);
    res.json::<Value>()["id"].as_i64().unwrap()
}

#[test_db::test(seed)]
async fn finished_templates_and_installs_are_paid_per_sq_ft_with_extras(pool: MySqlPool) {
    let server = server(pool.clone());
    finished_jobs(&pool).await;
    // No rule yet: nothing is paid per job.
    assert_eq!(sync(&server).await["jobs"], 0);
    assert_eq!(jobs(&server).await, json!([]));

    for bad in [
        json!({ "name": "", "work": "template", "kind": "sqft", "amount": 2 }),
        json!({ "name": "Template", "work": "sales", "kind": "sqft", "amount": 2 }),
        json!({ "name": "Template", "work": "template", "kind": "sqft", "amount": -1 }),
        json!({ "name": "Template", "work": "template", "kind": "sqft", "amount": 2, "userId": 999_999 }),
    ] {
        server
            .post("/v1/job-pay-rules")
            .with_session(sessions::ADMIN)
            .json(&bad)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    // $2 per sq ft for every Template technician, and a named extra.
    let template = add_rule(
        &server,
        json!({ "name": " Template ", "work": "template", "kind": "sqft", "amount": 2 }),
    )
    .await;
    add_rule(
        &server,
        json!({ "name": "Tear-out", "work": "template", "kind": "extra", "amount": 50 }),
    )
    .await;
    let rules = server
        .get("/v1/job-pay-rules")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>();
    assert_eq!(rules[0]["name"], "Template");
    assert_eq!(rules[0]["kind"], "sqft");
    assert_eq!(rules[1]["name"], "Tear-out");
    // The first rule starts the count today: yesterday's jobs are before it.
    assert_eq!(sync(&server).await["jobs"], 0);
    sqlx::query("UPDATE commission_settings SET job_pay_from = CURDATE() - INTERVAL 30 DAY")
        .execute(&pool)
        .await
        .unwrap();

    // The finished template pays its technician; the install has no rule,
    // and next week's template is not finished.
    assert_eq!(sync(&server).await["jobs"], 1);
    let list = jobs(&server).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    let job = &list[0];
    assert_eq!(job["eventId"], 88401);
    assert_eq!(job["work"], "template");
    assert_eq!(job["userId"], 101);
    assert_eq!(job["job"], "Template: Smith");
    assert_eq!(job["rate"], 2.0);
    // No quote on the event: the sq ft is not known yet.
    assert_eq!(job["sqFt"], Value::Null);
    assert_eq!(job["total"], 0.0);
    let id = job["id"].as_i64().unwrap();

    // An admin types the sq ft and adds extras.
    for bad in [
        json!({ "sqFt": -1 }),
        json!({ "sqFt": 50, "extras": [{ "label": " ", "amount": 5 }] }),
        json!({ "sqFt": 50, "payrollDate": "soon" }),
    ] {
        server
            .put(&format!("/v1/job-pay/{id}"))
            .with_session(sessions::ADMIN)
            .json(&bad)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    server
        .put(&format!("/v1/job-pay/{id}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "sqFt": 50.5, "extras": [
            { "label": "Tear-out", "amount": 50 }, { "label": " Extra trip ", "amount": 25 },
        ] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    // The sync keeps an admin's numbers.
    sync(&server).await;
    let job = &jobs(&server).await[0];
    assert_eq!(job["sqFt"], 50.5);
    assert_eq!(job["amount"], 101.0);
    assert_eq!(job["adjusted"], true);
    assert_eq!(
        job["extras"],
        json!([{ "label": "Tear-out", "amount": 50.0 }, { "label": "Extra trip", "amount": 25.0 }])
    );
    assert_eq!(job["total"], 176.0);
    assert_eq!(job["payrollDate"], Value::Null);

    // Into the salary: a payroll date, and the paycheck shows it.
    let added = server
        .post(&format!("/v1/job-pay/{id}/payroll"))
        .with_session(sessions::ADMIN)
        .await;
    added.assert_status_ok();
    let payroll = added.json::<Value>()["payrollDate"]
        .as_str()
        .unwrap()
        .to_string();
    server
        .post(&format!("/v1/job-pay/{id}/payroll"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let commissions = server
        .get("/v1/commissions?scope=company")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>();
    let week = commissions["weeks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["payrollDate"] == payroll.as_str())
        .unwrap();
    let paycheck = week["reps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["userId"] == 101)
        .unwrap();
    assert_eq!(paycheck["jobPay"], 176.0);
    assert_eq!(paycheck["total"], 176.0);

    // Paid out, and a rule change no longer moves a job in the salary.
    server
        .post("/v1/job-pay/payouts")
        .with_session(sessions::ADMIN)
        .json(&json!({ "ids": [id] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .put(&format!("/v1/job-pay-rules/{template}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Template", "work": "template", "kind": "sqft", "amount": 3 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    sync(&server).await;
    let job = &jobs(&server).await[0];
    assert_eq!(job["paidOut"], true);
    assert_eq!(job["rate"], 2.0);

    // One person's own rate: the rep's installs at $1.50, though no Installer.
    add_rule(
        &server,
        json!({ "name": "Rita's installs", "work": "install", "kind": "sqft", "amount": 1.5, "userId": 100 }),
    )
    .await;
    assert_eq!(sync(&server).await["jobs"], 2);
    let list = jobs(&server).await;
    let install = list
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["work"] == "install")
        .unwrap();
    assert_eq!(install["userId"], 100);
    assert_eq!(install["rate"], 1.5);

    // The settings say when job pay is paid.
    server
        .put("/v1/commission-settings")
        .with_session(sessions::ADMIN)
        .json(&json!({ "payrollWeekday": 5, "jobPaySameWeek": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let settings = server
        .get("/v1/commission-settings")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>();
    assert_eq!(settings["jobPaySameWeek"], true);
    assert!(settings["jobPayFrom"].is_string());

    // Only admins.
    server
        .get("/v1/job-pay")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}
