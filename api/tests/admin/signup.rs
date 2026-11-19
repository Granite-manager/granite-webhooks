//! Self sign-up (ENG-90): prices, the new company, and the paywall that
//! keeps it out until Stripe shows it paid.

use crate::common;

use api::app;
use api::state::AppState;
use axum::http::StatusCode;
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

fn server(pool: &MySqlPool) -> TestServer {
    TestServer::new(app(AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)))
}

fn signup(email: &str, subdomain: &str) -> Value {
    json!({
        "company_name": "Fresh Stone",
        "subdomain": subdomain,
        "address": "1 Main St",
        "name": "Fay Fresh",
        "email": email,
        "phone_number": "(317) 555-0199",
        "password": "a-good-password",
        "accept_terms": true,
    })
}

/// The new company and the session its manager signed in with.
async fn signed_up(pool: &MySqlPool, email: &str) -> (i32, String) {
    let (company_id, user_id): (i32, i32) =
        sqlx::query_as("SELECT company_id, id FROM users WHERE email = ?")
            .bind(email)
            .fetch_one(pool)
            .await
            .unwrap();
    let session: String =
        sqlx::query_scalar("SELECT id FROM sessions WHERE user_id = ? AND is_deleted = 0")
            .bind(user_id)
            .fetch_one(pool)
            .await
            .unwrap();
    (company_id, session)
}

async fn mark_paid(pool: &MySqlPool, company_id: i32, status: &str, payment: &str) {
    sqlx::query(
        "INSERT INTO company_billing (company_id, stripe_customer_id, stripe_subscription_id,
                                      subscription_status, payment_status, grace_period_ends_at)
         VALUES (?, ?, 'sub_signup', ?, ?, UTC_TIMESTAMP() - INTERVAL 1 DAY)
         ON DUPLICATE KEY UPDATE subscription_status = VALUES(subscription_status),
                                 payment_status = VALUES(payment_status)",
    )
    .bind(company_id)
    .bind(format!("cus_signup_{company_id}"))
    .bind(status)
    .bind(payment)
    .execute(pool)
    .await
    .unwrap();
}

#[test_db::test(seed)]
async fn signing_up_saves_everything_at_800_a_month(pool: MySqlPool) {
    let server = server(&pool);
    let options: Value = server.get("/v1/public/signup").await.json();
    assert_eq!(options["monthly_price_cents"], 80_000);
    assert_eq!(options["contact_email"], "sales@granite-manager.com");

    let check: Value = server
        .get("/v1/public/signup/subdomain/Fresh-Stone")
        .await
        .json();
    assert_eq!(check["available"], true);
    assert_eq!(check["subdomain"], "fresh-stone");

    let response = server
        .post("/v1/public/signup")
        .json(&signup(" Fay@Fresh.test ", "fresh-stone"))
        .await;
    response.assert_status_ok();
    let body: Value = response.json();
    assert_eq!(body["home_path"], "/employee");
    assert!(response.maybe_header("set-cookie").is_some());

    let (company_id, session) = signed_up(&pool, "fay@fresh.test").await;
    let (source, name, address, requested, subdomain, price, required): (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        i32,
        bool,
    ) = sqlx::query_as(
        "SELECT source, name, address, requested_subdomain, subdomain, subscription_price_cents,
                billing_required <> 0
           FROM company WHERE id = ?",
    )
    .bind(company_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(source, "signup_page");
    assert_eq!(name, "Fresh Stone");
    assert_eq!(address.as_deref(), Some("1 Main St"));
    assert_eq!(requested.as_deref(), Some("fresh-stone"));
    assert_eq!(subdomain, None);
    assert_eq!(price, 80_000);
    assert!(required);
    let (user_name, phone): (String, Option<String>) =
        sqlx::query_as("SELECT name, phone_number FROM users WHERE email = 'fay@fresh.test'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(user_name, "Fay Fresh");
    assert_eq!(phone.as_deref(), Some("(317) 555-0199"));
    let manager: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM users_positions up JOIN users u ON u.id = up.user_id
          WHERE u.email = 'fay@fresh.test' AND up.company_id = ? AND up.position_id = 5",
    )
    .bind(company_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(manager, 1);
    let calendars: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM calendars WHERE company_id = ?")
        .bind(company_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(calendars > 0, "seeded like a superuser's company");

    let first: Value = server
        .get("/v1/billing/first-payment")
        .with_session(&session)
        .await
        .json();
    assert_eq!(first["monthly_price_cents"], 80_000);
    assert_eq!(first["contact_email"], "sales@granite-manager.com");
    assert_eq!(first["free_access_until"], Value::Null);
    assert_eq!(first["subscribed"], false);
}

#[test_db::test(seed)]
async fn a_free_month_from_a_superuser_lets_it_in(pool: MySqlPool) {
    let server = server(&pool);
    server
        .post("/v1/public/signup")
        .json(&signup("fay@fresh.test", "fresh-stone"))
        .await
        .assert_status_ok();
    let (company_id, session) = signed_up(&pool, "fay@fresh.test").await;
    server
        .get("/v1/customers")
        .with_session(&session)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    sqlx::query("UPDATE users SET is_superuser = 1 WHERE id = 102")
        .execute(&pool)
        .await
        .unwrap();
    let path = format!("/v1/superuser/companies/{company_id}/free-access");
    // Only superusers, and only days from today on.
    server
        .put(&path)
        .with_session(&session)
        .json(&json!({ "until": "2099-01-01" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .put(&path)
        .with_session(sessions::ADMIN)
        .json(&json!({ "until": "2020-01-01" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let in_a_month = (chrono::Utc::now() + chrono::Duration::days(30))
        .date_naive()
        .to_string();
    server
        .put(&path)
        .with_session(sessions::ADMIN)
        .json(&json!({ "until": in_a_month }))
        .await
        .assert_status_ok();
    server
        .get("/v1/customers")
        .with_session(&session)
        .await
        .assert_status_ok();
    let gate: Value = server
        .get("/v1/billing/gate")
        .with_session(&session)
        .await
        .json();
    assert_eq!(gate["restricted"], false);
    let first: Value = server
        .get("/v1/billing/first-payment")
        .with_session(&session)
        .await
        .json();
    assert!(first["free_access_until"].is_string());
    let companies: Value = server
        .get("/v1/superuser/companies")
        .with_session(sessions::ADMIN)
        .await
        .json();
    let listed = companies
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == company_id)
        .unwrap();
    assert_eq!(listed["source"], "signup_page");
    assert!(listed["free_access_until"].is_string());

    // Free time over (ended by hand here): shut again, without any cache.
    sqlx::query(
        "UPDATE company SET free_access_until = UTC_TIMESTAMP() - INTERVAL 1 MINUTE WHERE id = ?",
    )
    .bind(company_id)
    .execute(&pool)
    .await
    .unwrap();
    let gate: Value = server
        .get("/v1/billing/gate")
        .with_session(&session)
        .await
        .json();
    assert_eq!(gate["restricted"], true);
    assert_eq!(gate["first_payment_due"], true);
}

#[test_db::test(seed)]
async fn an_unpaid_company_gets_nothing_but_billing(pool: MySqlPool) {
    let server = server(&pool);
    server
        .post("/v1/public/signup")
        .json(&signup("fay@fresh.test", "fresh-stone"))
        .await
        .assert_status_ok();
    let (company_id, session) = signed_up(&pool, "fay@fresh.test").await;

    for path in [
        "/v1/customers",
        "/v1/company/setup-checklist",
        "/v1/stones",
        "/v1/deals",
    ] {
        server
            .get(path)
            .with_session(&session)
            .await
            .assert_status(StatusCode::FORBIDDEN);
    }
    for path in ["/v1/auth/session", "/v1/billing/gate", "/v1/app/shell"] {
        server
            .get(path)
            .with_session(&session)
            .await
            .assert_status_ok();
    }
    let gate: Value = server
        .get("/v1/billing/gate")
        .with_session(&session)
        .await
        .json();
    assert_eq!(gate["restricted"], true);
    assert_eq!(gate["first_payment_due"], true);
    assert_eq!(gate["can_manage"], true);

    // The billing switch does not open it.
    sqlx::query("UPDATE company SET billing_required = 0 WHERE id = ?")
        .bind(company_id)
        .execute(&pool)
        .await
        .unwrap();
    server
        .get("/v1/customers")
        .with_session(&session)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    // Nor does a Checkout that never paid, or a subscription that ended.
    mark_paid(&pool, company_id, "incomplete", "none").await;
    server
        .get("/v1/customers")
        .with_session(&session)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    mark_paid(&pool, company_id, "canceled", "active").await;
    server
        .get("/v1/customers")
        .with_session(&session)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    // Nor a renewal that failed and whose grace period is over.
    mark_paid(&pool, company_id, "past_due", "past_due").await;
    server
        .get("/v1/customers")
        .with_session(&session)
        .await
        .assert_status(StatusCode::FORBIDDEN);

    // Stripe shows it paid: in at once.
    mark_paid(&pool, company_id, "active", "active").await;
    server
        .get("/v1/customers")
        .with_session(&session)
        .await
        .assert_status_ok();
    let gate: Value = server
        .get("/v1/billing/gate")
        .with_session(&session)
        .await
        .json();
    assert_eq!(gate["restricted"], false);

    // Companies a superuser created never meet the paywall.
    server
        .get("/v1/customers")
        .with_session(sessions::REP)
        .await
        .assert_status_ok();
}

#[test_db::test(seed)]
async fn bad_and_repeated_sign_ups_are_refused(pool: MySqlPool) {
    let server = server(&pool);
    // An email that already signs in somewhere.
    let taken_email: String = sqlx::query_scalar("SELECT email FROM users WHERE id = 100")
        .fetch_one(&pool)
        .await
        .unwrap();
    server
        .post("/v1/public/signup")
        .json(&signup(&taken_email.to_uppercase(), "fresh-stone"))
        .await
        .assert_status(StatusCode::CONFLICT);
    // A web address another company has.
    let taken: Option<String> = sqlx::query_scalar("SELECT subdomain FROM company WHERE id = 100")
        .fetch_one(&pool)
        .await
        .unwrap();
    if let Some(taken) = taken {
        server
            .post("/v1/public/signup")
            .json(&signup("new@fresh.test", &taken))
            .await
            .assert_status(StatusCode::CONFLICT);
    }
    // The honeypot, and no terms.
    let mut bot = signup("bot@fresh.test", "bot-stone");
    bot["website"] = json!("http://spam.test");
    server
        .post("/v1/public/signup")
        .json(&bot)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let mut no_terms = signup("terms@fresh.test", "terms-stone");
    no_terms["accept_terms"] = json!(false);
    server
        .post("/v1/public/signup")
        .json(&no_terms)
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    // Three a day from one address, then 429.
    for n in 0..3 {
        server
            .post("/v1/public/signup")
            .add_header("x-forwarded-for", "203.0.113.77")
            .json(&signup(&format!("n{n}@fresh.test"), &format!("fresh-{n}")))
            .await
            .assert_status_ok();
    }
    server
        .post("/v1/public/signup")
        .add_header("x-forwarded-for", "203.0.113.77")
        .json(&signup("n9@fresh.test", "fresh-9"))
        .await
        .assert_status(StatusCode::TOO_MANY_REQUESTS);
    // The asked-for address is now taken too.
    let check: Value = server
        .get("/v1/public/signup/subdomain/fresh-0")
        .await
        .json();
    assert_eq!(check["available"], false);
}

#[test_db::test(seed)]
async fn superusers_set_feature_prices(pool: MySqlPool) {
    sqlx::query("UPDATE users SET is_superuser = 1 WHERE id = 102")
        .execute(&pool)
        .await
        .unwrap();
    let server = server(&pool);
    let id: i32 = sqlx::query_scalar("SELECT id FROM features WHERE name = 'Texting'")
        .fetch_one(&pool)
        .await
        .unwrap();
    server
        .put(&format!("/v1/superuser/paid-features/{id}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Texting", "monthly_price_cents": 9_900 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    // A rename without a price keeps it.
    server
        .put(&format!("/v1/superuser/paid-features/{id}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Texts" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .put(&format!("/v1/superuser/paid-features/{id}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Texts", "monthly_price_cents": 10 }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let list: Value = server
        .get("/v1/superuser/paid-features")
        .with_session(sessions::ADMIN)
        .await
        .json();
    let texts = list["features"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == id)
        .unwrap();
    assert_eq!(texts["name"], "Texts");
    assert_eq!(texts["monthly_price_cents"], 9_900);
    assert_eq!(list["chosen"], json!([]));
}

#[test_db::test(seed)]
async fn the_setup_checklist_is_for_managers(pool: MySqlPool) {
    sqlx::query(
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (101, 5, 100)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(&pool);
    server
        .get("/v1/company/setup-checklist")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let response = server
        .get("/v1/company/setup-checklist")
        .with_session(sessions::MANAGER)
        .await;
    response.assert_status_ok();
    let list: Value = response.json();
    assert_eq!(list["contact_email"], "support@granite-manager.com");
    let item = |list: &Value, key: &str| {
        list["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["key"] == key)
            .cloned()
            .unwrap_or_else(|| panic!("no {key}"))
    };
    let phone = item(&list, "phone_line");
    assert_eq!(phone["how"], "contact");
    assert_eq!(phone["href"], Value::Null);
    assert_eq!(item(&list, "team")["how"], "self");
    let lead = item(&list, "lead_form");
    let ticked: Value = server
        .put("/v1/company/setup-checklist/lead_form")
        .with_session(sessions::MANAGER)
        .json(&json!({ "ticked": true }))
        .await
        .json();
    let after = item(&ticked, "lead_form");
    assert_eq!(after["ticked"], true);
    assert_eq!(after["done"], true);
    if lead["done"] == false {
        assert_eq!(
            ticked["done_count"].as_i64().unwrap(),
            list["done_count"].as_i64().unwrap() + 1
        );
    }
    let unticked: Value = server
        .put("/v1/company/setup-checklist/lead_form")
        .with_session(sessions::MANAGER)
        .json(&json!({ "ticked": false }))
        .await
        .json();
    assert_eq!(item(&unticked, "lead_form")["ticked"], false);
    server
        .put("/v1/company/setup-checklist/nope")
        .with_session(sessions::MANAGER)
        .json(&json!({ "ticked": true }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
}
