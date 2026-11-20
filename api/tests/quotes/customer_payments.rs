//! Customer payments (`routes/customer_payments.rs`) against a fake Stripe.
//! Seed: company 100 Acme Stone. Alice (1000) has the won deals 8000 and
//! 8004; Erin (1004) has the sales 7000 and 7001 and the child Frank (1005)
//! with the sale 7002; customer 2000 and sale 7003 are company 101's. Ada
//! Admin (102) is made a Manager, so she may change the settings and hears
//! about every payment.

use crate::common;

use api::app;
use api::billing::stripe::{Stripe, signature_header};
use api::routes::customer_payments::CustomerPayments;
use api::state::AppState;
use api::webhooks_client::{OutgoingEmail, WebhooksClient};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const WEBHOOK_SECRET: &str = "whsec_connect_test";
const ACCOUNT: &str = "acct_1";
const VIEW: &str = "3b1f6c2e-9d4a-4e8b-a1c7-5f2e8d9c0b1a";

/// A stand-in for the Stripe API.
#[derive(Default)]
struct FakeStripe {
    /// Connected accounts by id.
    accounts: HashMap<String, Value>,
    /// Checkout sessions by id.
    sessions: HashMap<String, Value>,
    /// Payment intents by id.
    intents: HashMap<String, Value>,
    /// (method, path, `Stripe-Account`, form)
    requests: Vec<(String, String, Option<String>, String)>,
}

impl FakeStripe {
    fn count(&self, method: &str, path: &str) -> usize {
        self.requests
            .iter()
            .filter(|(m, p, _, _)| m == method && p == path)
            .count()
    }

    /// The last request to `path`: its `Stripe-Account` and form.
    fn last(&self, method: &str, path: &str) -> (Option<String>, HashMap<String, String>) {
        let (_, _, account, body) = self
            .requests
            .iter()
            .rev()
            .find(|(m, p, _, _)| m == method && p == path)
            .unwrap_or_else(|| panic!("no {method} {path}"));
        (account.clone(), serde_html_form::from_str(body).unwrap())
    }
}

type Shared = Arc<Mutex<FakeStripe>>;
type Reply = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn missing(what: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": { "message": format!("No such {what}") } })),
    )
}

fn account_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get("stripe-account")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

async fn fake_stripe(state: Shared) -> String {
    let router = Router::new()
        .route(
            "/v2/core/accounts",
            post(|State(s): State<Shared>, body: String| async move {
                let mut s = s.lock().unwrap();
                s.requests
                    .push(("POST".into(), "/v2/core/accounts".into(), None, body));
                let id = format!("acct_{}", s.accounts.len() + 1);
                let account = json!({ "id": id, "object": "account", "charges_enabled": false });
                s.accounts.insert(id, account.clone());
                Json(account)
            }),
        )
        .route(
            "/v1/accounts/{id}",
            get(|State(s): State<Shared>, Path(id): Path<String>| async move {
                let mut s = s.lock().unwrap();
                s.requests
                    .push(("GET".into(), format!("/v1/accounts/{id}"), None, String::new()));
                s.accounts
                    .get(&id)
                    .cloned()
                    .map(Json)
                    .ok_or_else(|| missing("account"))
            }),
        )
        .route(
            "/v1/account_links",
            post(|State(s): State<Shared>, body: String| async move {
                let mut s = s.lock().unwrap();
                s.requests
                    .push(("POST".into(), "/v1/account_links".into(), None, body));
                Json(json!({ "object": "account_link", "url": "https://connect.stripe.com/setup/s/1" }))
            }),
        )
        .route(
            "/v1/checkout/sessions",
            post(
                |State(s): State<Shared>, headers: HeaderMap, body: String| async move {
                    let mut s = s.lock().unwrap();
                    s.requests.push((
                        "POST".into(),
                        "/v1/checkout/sessions".into(),
                        account_header(&headers),
                        body,
                    ));
                    let id = format!("cs_{}", s.sessions.len() + 1);
                    let session = json!({ "id": id, "status": "open",
                        "url": format!("https://checkout.stripe.com/c/pay/{id}") });
                    s.sessions.insert(id, session.clone());
                    Json(session)
                },
            ),
        )
        .route(
            "/v1/checkout/sessions/{id}",
            get(
                |State(s): State<Shared>, headers: HeaderMap, Path(id): Path<String>| async move {
                    let mut s = s.lock().unwrap();
                    s.requests.push((
                        "GET".into(),
                        "/v1/checkout/sessions/{id}".into(),
                        account_header(&headers),
                        String::new(),
                    ));
                    let reply: Reply = s
                        .sessions
                        .get(&id)
                        .cloned()
                        .map(Json)
                        .ok_or_else(|| missing("session"));
                    reply
                },
            ),
        )
        .route(
            "/v1/checkout/sessions/{id}/expire",
            post(
                |State(s): State<Shared>, headers: HeaderMap, Path(id): Path<String>| async move {
                    let mut s = s.lock().unwrap();
                    s.requests.push((
                        "POST".into(),
                        "/v1/checkout/sessions/{id}/expire".into(),
                        account_header(&headers),
                        String::new(),
                    ));
                    let session = s.sessions.get_mut(&id).ok_or_else(|| missing("session"))?;
                    session["status"] = json!("expired");
                    session["url"] = Value::Null;
                    Ok::<_, (StatusCode, Json<Value>)>(Json(session.clone()))
                },
            ),
        )
        .route(
            "/v1/payment_intents/{id}",
            get(
                |State(s): State<Shared>, headers: HeaderMap, Path(id): Path<String>| async move {
                    let mut s = s.lock().unwrap();
                    s.requests.push((
                        "GET".into(),
                        "/v1/payment_intents/{id}".into(),
                        account_header(&headers),
                        String::new(),
                    ));
                    let reply: Reply = s
                        .intents
                        .get(&id)
                        .cloned()
                        .map(Json)
                        .ok_or_else(|| missing("payment intent"));
                    reply
                },
            ),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{address}")
}

async fn make_manager(pool: &MySqlPool) {
    sqlx::query(
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (102, 5, 100)",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// The api against a fake Stripe, with Ada a Manager.
async fn server(pool: &MySqlPool) -> (TestServer, Shared) {
    make_manager(pool).await;
    let shared = Shared::default();
    let stripe = Stripe::new(
        Some("sk_test_fake"),
        &fake_stripe(Arc::clone(&shared)).await,
    );
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_stripe(stripe)
        .with_extension(CustomerPayments::new(WEBHOOK_SECRET, false));
    (TestServer::new(app(state)), shared)
}

/// Company 100 with a bank account Stripe approved.
async fn connect_bank(pool: &MySqlPool) {
    sqlx::query(
        "INSERT INTO company_payment_settings
                (company_id, stripe_account_id, charges_enabled, bank_name, bank_last4)
         VALUES (100, ?, 1, 'Chase', '6789')",
    )
    .bind(ACCOUNT)
    .execute(pool)
    .await
    .unwrap();
}

async fn connected_server(pool: &MySqlPool) -> (TestServer, Shared) {
    let pair = server(pool).await;
    connect_bank(pool).await;
    pair
}

/// A payment link made by Rita Rep; its JSON.
async fn link(server: &TestServer, customer: i32, key: &str, method: &str, cents: i64) -> Value {
    let response = server
        .post(&format!("/v1/customers/{customer}/payments"))
        .with_session(sessions::REP)
        .json(&json!({ "project_key": key, "method": method, "amount_cents": cents }))
        .await;
    response.assert_status(StatusCode::CREATED);
    response.json()
}

async fn status_of(pool: &MySqlPool, id: &str) -> String {
    sqlx::query_scalar("SELECT CAST(status AS CHAR) FROM customer_payments WHERE public_id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The bell rows about payments: (user, type, message), oldest first.
async fn bell(pool: &MySqlPool) -> Vec<(i32, String, String)> {
    sqlx::query_as(
        "SELECT user_id, notification_type, message FROM notifications
          WHERE notification_type LIKE 'payment%' ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Sends Stripe's event about `account`, signed, with an id of its own.
async fn webhook(server: &TestServer, kind: &str, account: &str, object: Value) -> StatusCode {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let event = format!("evt_{}", NEXT.fetch_add(1, Ordering::Relaxed));
    webhook_event(server, &event, kind, account, object).await
}

/// Sends Stripe's event `event` about `account`, signed: the same id twice
/// is Stripe sending one event again.
async fn webhook_event(
    server: &TestServer,
    event: &str,
    kind: &str,
    account: &str,
    object: Value,
) -> StatusCode {
    let body = json!({ "id": event, "type": kind, "account": account,
                       "data": { "object": object } })
    .to_string();
    let signature = signature_header(
        body.as_bytes(),
        WEBHOOK_SECRET,
        chrono::Utc::now().timestamp(),
    );
    server
        .post("/v1/webhooks/stripe-connect")
        .add_header("stripe-signature", signature)
        .add_header("content-type", "application/json")
        .bytes(body.into())
        .await
        .status_code()
}

fn session_event(payment: &str, payment_status: &str, intent: &str) -> Value {
    json!({ "id": "cs_1", "object": "checkout.session", "payment_status": payment_status,
            "payment_intent": intent, "metadata": { "granite_payment": payment } })
}

// ---- settings

#[test_db::test(seed)]
async fn a_company_starts_without_a_bank_account(pool: MySqlPool) {
    let (server, stripe) = server(&pool).await;
    let settings = server
        .get("/v1/company/payment-settings")
        .with_session(sessions::REP)
        .await;
    settings.assert_status_ok();
    assert_eq!(
        settings.json::<Value>(),
        json!({
            "bank": { "status": "not_connected", "bank_name": null, "last4": null },
            "fees": {
                "card": { "rate_bps": 300, "customer_share_bps": 0, "min_fee_cents": 900,
                          "locked": true },
                "ach": { "rate_bps": 100, "customer_share_bps": 0, "min_fee_cents": 0,
                         "locked": false },
            },
            "enabled": true,
        })
    );
    assert!(stripe.lock().unwrap().requests.is_empty());

    server
        .get("/v1/company/payment-settings")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .get("/v1/company/payment-settings")
        .with_session(sessions::VIEWER)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn only_a_manager_admin_sets_who_pays_the_fees(pool: MySqlPool) {
    let (server, _) = server(&pool).await;
    let fees = json!({ "card_customer_share_bps": 0, "ach_customer_share_bps": 40 });
    server
        .put("/v1/company/payment-settings")
        .with_session(sessions::REP)
        .json(&fees)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .post("/v1/company/payment-settings/bank-account")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);

    server
        .put("/v1/company/payment-settings")
        .with_session(sessions::ADMIN)
        .json(&fees)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let settings: Value = server
        .get("/v1/company/payment-settings")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(settings["fees"]["ach"]["customer_share_bps"], 40);
    assert_eq!(settings["fees"]["card"]["customer_share_bps"], 0);

    // The card fee stays the company's; ACH is 0% to 1% in 0.1% steps.
    for (card, ach) in [(10, 0), (0, 110), (0, -10), (0, 45)] {
        let refused = server
            .put("/v1/company/payment-settings")
            .with_session(sessions::ADMIN)
            .json(&json!({ "card_customer_share_bps": card, "ach_customer_share_bps": ach }))
            .await;
        refused.assert_status(StatusCode::BAD_REQUEST);
    }
    // Another company's settings are its own.
    let other: Value = server
        .get("/v1/company/payment-settings")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json();
    assert_eq!(other["fees"]["ach"]["customer_share_bps"], 0);
}

#[test_db::test(seed)]
async fn connecting_a_bank_account_goes_through_stripe(pool: MySqlPool) {
    let (server, stripe) = server(&pool).await;
    let started = server
        .post("/v1/company/payment-settings/bank-account")
        .with_session(sessions::ADMIN)
        .await;
    started.assert_status_ok();
    assert_eq!(
        started.json::<Value>(),
        json!({ "url": "https://connect.stripe.com/setup/s/1" })
    );
    {
        let stripe = stripe.lock().unwrap();
        // Accounts v2 takes JSON, not a form.
        let (_, _, _, body) = stripe
            .requests
            .iter()
            .find(|(_, path, _, _)| path == "/v2/core/accounts")
            .expect("no account made");
        let account: Value = serde_json::from_str(body).unwrap();
        assert_eq!(account["display_name"], "Acme Stone");
        assert_eq!(account["dashboard"], "full");
        assert_eq!(
            account["defaults"]["responsibilities"],
            json!({ "fees_collector": "stripe", "losses_collector": "stripe" })
        );
        assert_eq!(
            account["configuration"]["merchant"]["capabilities"],
            json!({ "card_payments": { "requested": true },
                    "ach_debit_payments": { "requested": true } })
        );
        assert_eq!(account["metadata"]["company_id"], "100");
        let (_, link) = stripe.last("POST", "/v1/account_links");
        assert_eq!(link["account"], ACCOUNT);
        assert_eq!(link["type"], "account_onboarding");
        assert_eq!(
            link["return_url"],
            "http://localhost:5173/admin/company?payments=return"
        );
    }

    // Stripe has not approved it yet: asked now, then at most once a minute
    // for the whole company, however many pages read the settings.
    let asked = |stripe: &Shared| stripe.lock().unwrap().count("GET", "/v1/accounts/acct_1");
    let settings = || async {
        server
            .get("/v1/company/payment-settings")
            .with_session(sessions::REP)
            .await
            .json::<Value>()
    };
    assert_eq!(
        settings().await["bank"],
        json!({ "status": "pending_verification", "bank_name": null, "last4": null })
    );
    stripe.lock().unwrap().accounts.insert(
        ACCOUNT.into(),
        json!({ "id": ACCOUNT, "charges_enabled": true,
                "external_accounts": { "data": [{ "bank_name": "Chase", "last4": "6789" }] } }),
    );
    assert_eq!(asked(&stripe), 1);
    for _ in 0..3 {
        assert_eq!(settings().await["bank"]["status"], "pending_verification");
    }
    assert_eq!(asked(&stripe), 1);
    sqlx::query(
        "UPDATE company_payment_settings
            SET stripe_checked_at = UTC_TIMESTAMP() - INTERVAL 61 SECOND",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        settings().await["bank"],
        json!({ "status": "connected", "bank_name": "Chase", "last4": "6789" })
    );
    assert_eq!(asked(&stripe), 2);
    // Connected: Stripe is not asked again.
    let reads = stripe.lock().unwrap().count("GET", "/v1/accounts/acct_1");
    settings().await;
    assert_eq!(
        stripe.lock().unwrap().count("GET", "/v1/accounts/acct_1"),
        reads
    );

    // Starting again continues with the same account.
    server
        .post("/v1/company/payment-settings/bank-account")
        .with_session(sessions::ADMIN)
        .await
        .assert_status_ok();
    assert_eq!(stripe.lock().unwrap().count("POST", "/v2/core/accounts"), 1);
    // Back from Stripe's page the first read asks at once, minute or not.
    let checked: Option<chrono::NaiveDateTime> = sqlx::query_scalar(
        "SELECT stripe_checked_at FROM company_payment_settings WHERE company_id = 100",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(checked, None);
}

#[test_db::test(seed)]
async fn an_account_update_from_stripe_connects_the_bank(pool: MySqlPool) {
    let (server, _) = server(&pool).await;
    sqlx::query(
        "INSERT INTO company_payment_settings (company_id, stripe_account_id) VALUES (100, ?)",
    )
    .bind(ACCOUNT)
    .execute(&pool)
    .await
    .unwrap();
    let status = webhook(
        &server,
        "account.updated",
        ACCOUNT,
        json!({ "id": ACCOUNT, "charges_enabled": true }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let enabled: bool = sqlx::query_scalar(
        "SELECT charges_enabled FROM company_payment_settings WHERE company_id = 100",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(enabled);
}

// ---- payment links

#[test_db::test(seed)]
async fn a_link_needs_a_connected_bank_account(pool: MySqlPool) {
    let (server, _) = server(&pool).await;
    let refused = server
        .post("/v1/customers/1000/payments")
        .with_session(sessions::REP)
        .json(&json!({ "project_key": "deal-8000", "method": "card", "amount_cents": 50_000 }))
        .await;
    refused.assert_status(StatusCode::CONFLICT);
    assert_eq!(
        refused.json::<Value>()["error"],
        "Connect the company's bank account first (Admin → Company → Payments)"
    );
}

#[test_db::test(seed)]
async fn a_link_is_for_an_amount_of_one_job(pool: MySqlPool) {
    let (server, _) = connected_server(&pool).await;
    sqlx::query("UPDATE company_payment_settings SET ach_customer_share_bps = 40")
        .execute(&pool)
        .await
        .unwrap();

    let card = link(&server, 1000, "deal-8000", "card", 100_000).await;
    let id = card["id"].as_str().unwrap();
    assert!(id.starts_with("pay_") && id.len() == 36, "{id}");
    assert_eq!(
        card,
        json!({
            "id": id, "project_key": "deal-8000", "method": "card", "status": "created",
            "amount_cents": 100_000, "fee_cents": 3_000, "customer_fee_cents": 0,
            "customer_fee_bps": 0, "company_fee_cents": 3_000, "refunded_cents": 0,
            "url": format!("http://localhost:5173/pay/{id}"),
            "failure_reason": null, "created_at": card["created_at"],
        })
    );
    // The platform keeps what Stripe's usual fee leaves of the 3%.
    let kept: (i32, String, i32) = sqlx::query_as(
        "SELECT application_fee_cents, stripe_account_id, created_by
           FROM customer_payments WHERE public_id = ?",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kept, (70, ACCOUNT.to_string(), 100));

    // ACH with the customer paying 0.4% of the 1%.
    let ach = link(&server, 1000, "deal-8004", "ach", 100_000).await;
    assert_eq!(
        (
            &ach["fee_cents"],
            &ach["customer_fee_cents"],
            &ach["company_fee_cents"]
        ),
        (&json!(1_000), &json!(400), &json!(600))
    );

    // Newest first.
    let listed: Value = server
        .get("/v1/customers/1000/payments")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(listed, json!([ach, card]));
}

/// Deal 8000 costs $1,000: links cannot ask for more than is left of it.
#[test_db::test(seed)]
async fn a_link_cannot_ask_for_more_than_is_left_to_pay(pool: MySqlPool) {
    let (server, _) = connected_server(&pool).await;
    let refused = |cents: i64| {
        server
            .post("/v1/customers/1000/payments")
            .with_session(sessions::REP)
            .json(&json!({ "project_key": "deal-8000", "method": "card", "amount_cents": cents }))
    };

    let response = refused(100_001).await;
    response.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        response.json::<Value>()["error"],
        "The open balance is $1,000"
    );

    // An unpaid link takes its part of the balance.
    let first = link(&server, 1000, "deal-8000", "card", 60_000).await;
    let response = refused(40_001).await;
    response.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        response.json::<Value>()["error"],
        "Unpaid links already ask for $600: this one can be for up to $400"
    );

    // Paid, it still does; refunded, it gives the balance back.
    let id = first["id"].as_str().unwrap();
    webhook(
        &server,
        "checkout.session.completed",
        ACCOUNT,
        session_event(id, "paid", "pi_1"),
    )
    .await;
    assert_eq!(status_of(&pool, id).await, "paid");
    let response = refused(40_001).await;
    response.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        response.json::<Value>()["error"],
        "The open balance is $400"
    );
    link(&server, 1000, "deal-8000", "card", 40_000).await;

    // A withdrawn link asks for nothing.
    sqlx::query("UPDATE customer_payments SET status = 'canceled' WHERE status = 'created'")
        .execute(&pool)
        .await
        .unwrap();
    link(&server, 1000, "deal-8000", "ach", 40_000).await;
}

#[test_db::test(seed)]
async fn a_link_is_only_for_the_family_s_own_jobs(pool: MySqlPool) {
    let (server, _) = connected_server(&pool).await;
    // Frank (1005) is Erin's (1004) project: either page pays either's sale.
    let sale = link(&server, 1005, "sale-7000", "card", 20_000).await;
    assert_eq!(sale["project_key"], "sale-7000");
    link(&server, 1004, "sale-7002", "ach", 20_000).await;
    for customer in [1004, 1005] {
        let listed: Value = server
            .get(&format!("/v1/customers/{customer}/payments"))
            .with_session(sessions::REP)
            .await
            .json();
        assert_eq!(listed.as_array().unwrap().len(), 2, "{customer}");
    }
    // The payment is the job's own customer's.
    let owner: i32 =
        sqlx::query_scalar("SELECT customer_id FROM customer_payments WHERE sale_id = 7000")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(owner, 1004);

    // Another customer's deal, a deleted deal, another company's sale, junk.
    for key in [
        "deal-8000",
        "deal-8005",
        "sale-7003",
        "sale-1",
        "deal-x",
        "8000",
        "",
    ] {
        let refused = server
            .post("/v1/customers/1004/payments")
            .with_session(sessions::REP)
            .json(&json!({ "project_key": key, "method": "card", "amount_cents": 20_000 }))
            .await;
        refused.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(refused.json::<Value>()["error"], "Unknown project", "{key}");
    }
    // A card payment under $300 costs the $9 minimum; under $10 it is refused.
    let small = link(&server, 1004, "sale-7000", "card", 10_000).await;
    assert_eq!(
        (&small["fee_cents"], &small["company_fee_cents"]),
        (&json!(900), &json!(900))
    );
    let tiny = server
        .post("/v1/customers/1004/payments")
        .with_session(sessions::REP)
        .json(&json!({ "project_key": "sale-7000", "method": "card", "amount_cents": 999 }))
        .await;
    tiny.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        tiny.json::<Value>()["error"],
        "A card payment must be at least $10. Use ACH for a smaller amount"
    );
    link(&server, 1004, "sale-7000", "ach", 999).await;
    for cents in [0, 99, 100_000_000, -5] {
        server
            .post("/v1/customers/1004/payments")
            .with_session(sessions::REP)
            .json(&json!({ "project_key": "sale-7000", "method": "card", "amount_cents": cents }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    // Another company's customer, and another company's caller.
    server
        .post("/v1/customers/2000/payments")
        .with_session(sessions::REP)
        .json(&json!({ "project_key": "sale-7003", "method": "card", "amount_cents": 20_000 }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .get("/v1/customers/1004/payments")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .get("/v1/customers/1004/payments")
        .with_session(sessions::VIEWER)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn a_sale_s_payment_follows_it_into_its_deal(pool: MySqlPool) {
    let (server, _) = connected_server(&pool).await;
    link(&server, 1004, "sale-7000", "card", 20_000).await;
    sqlx::query("UPDATE sales SET deal_id = 8002 WHERE id = 7000")
        .execute(&pool)
        .await
        .unwrap();
    let listed: Value = server
        .get("/v1/customers/1004/payments")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(listed[0]["project_key"], "deal-8002");
}

// ---- the payment page

#[test_db::test(seed)]
async fn the_page_sends_the_customer_to_stripe_checkout(pool: MySqlPool) {
    let (server, stripe) = connected_server(&pool).await;
    sqlx::raw_sql(
        "UPDATE company_payment_settings SET ach_customer_share_bps = 100;
         -- The deal's quotes: the signed one is the job's, though older.
         INSERT INTO quotes (id, customer_id, quote_type, created_date, sales_rep, company_id,
                             deal_id, signed_at)
         VALUES (9100, 1000, 'kitchen', '2026-01-10', 'Rita Rep', 100, 8000, '2026-01-12'),
                (9101, 1000, 'kitchen', '2026-01-20', 'Rita Rep', 100, 8000, NULL);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let payment = link(&server, 1000, "deal-8000", "ach", 100_000).await;
    let id = payment["id"].as_str().unwrap();

    let page = server.get(&format!("/v1/public/pay/{id}")).await;
    page.assert_status_ok();
    assert_eq!(page.header("cache-control"), "private, no-store");
    assert_eq!(
        page.json::<Value>(),
        json!({ "company_name": "Acme Stone", "project_name": "Alice kitchen", "method": "ach",
                "status": "created", "amount_cents": 100_000, "customer_fee_cents": 1_000,
                "customer_fee_bps": 100, "total_cents": 101_000, "demo": false })
    );
    server
        .get("/v1/public/pay/pay_00000000000000000000000000000000")
        .await
        .assert_status(StatusCode::NOT_FOUND);

    let checkout = server.post(&format!("/v1/public/pay/{id}/checkout")).await;
    checkout.assert_status_ok();
    assert_eq!(
        checkout.json::<Value>(),
        json!({ "url": "https://checkout.stripe.com/c/pay/cs_1" })
    );
    {
        let stripe = stripe.lock().unwrap();
        let (account, form) = stripe.last("POST", "/v1/checkout/sessions");
        // A direct charge on the company's account.
        assert_eq!(account.as_deref(), Some(ACCOUNT));
        assert_eq!(form["mode"], "payment");
        assert_eq!(form["payment_method_types[0]"], "us_bank_account");
        assert_eq!(form["wallet_options[link][display]"], "never");
        assert_eq!(form["adaptive_pricing[enabled]"], "false");
        // The US-cards notice is for card pages only.
        assert!(!form.contains_key("custom_text[submit][message]"));
        // The customer signs in to their bank: no account numbers typed by
        // hand, which Stripe would confirm with micro-deposits over days.
        assert_eq!(
            form["payment_method_options[us_bank_account][verification_method]"],
            "instant"
        );
        assert_eq!(form["metadata[granite_payment]"], id);
        assert_eq!(form["payment_intent_data[metadata][granite_payment]"], id);
        // Who and what it is for, on the session and on the payment.
        for (key, value) in [
            ("company_id", "100"),
            ("customer_id", "1000"),
            ("project_key", "deal-8000"),
            ("deal_id", "8000"),
            ("quote_id", "9100"),
        ] {
            assert_eq!(form[&format!("metadata[{key}]")], value, "{key}");
            assert_eq!(
                form[&format!("payment_intent_data[metadata][{key}]")],
                value,
                "{key}"
            );
        }
        assert!(!form.contains_key("metadata[sale_id]"));
        assert_eq!(form["line_items[0][price_data][unit_amount]"], "100000");
        assert_eq!(
            form["line_items[0][price_data][product_data][name]"],
            "Alice kitchen · Project #8000"
        );
        assert_eq!(form["line_items[1][price_data][unit_amount]"], "1000");
        assert_eq!(
            form["line_items[1][price_data][product_data][name]"],
            "Processing fee (1%)"
        );
        // 1% less Stripe's $5 cap.
        assert_eq!(form["payment_intent_data[application_fee_amount]"], "500");
        assert_eq!(
            form["success_url"],
            format!("http://localhost:5173/pay/{id}?checkout=success")
        );
    }

    // The open page is reused, so the payment is not made twice.
    let again = server.post(&format!("/v1/public/pay/{id}/checkout")).await;
    assert_eq!(
        again.json::<Value>()["url"],
        "https://checkout.stripe.com/c/pay/cs_1"
    );
    assert_eq!(
        stripe
            .lock()
            .unwrap()
            .count("POST", "/v1/checkout/sessions"),
        1
    );

    // Stripe let it expire: a fresh one.
    stripe.lock().unwrap().sessions.get_mut("cs_1").unwrap()["status"] = json!("expired");
    let fresh = server.post(&format!("/v1/public/pay/{id}/checkout")).await;
    assert_eq!(
        fresh.json::<Value>()["url"],
        "https://checkout.stripe.com/c/pay/cs_2"
    );

    // Paid on Stripe, the webhook not here yet.
    stripe.lock().unwrap().sessions.get_mut("cs_2").unwrap()["status"] = json!("complete");
    let paid = server.post(&format!("/v1/public/pay/{id}/checkout")).await;
    paid.assert_status(StatusCode::CONFLICT);
    assert_eq!(
        paid.json::<Value>()["error"],
        "This payment was already made"
    );
}

#[test_db::test(seed)]
async fn a_withdrawn_link_cannot_be_paid(pool: MySqlPool) {
    let (server, stripe) = connected_server(&pool).await;
    let payment = link(&server, 1000, "deal-8000", "card", 50_000).await;
    let id = payment["id"].as_str().unwrap();
    server
        .post(&format!("/v1/public/pay/{id}/checkout"))
        .await
        .assert_status_ok();

    // Not through another customer or company.
    for (customer, session) in [(1004, sessions::REP), (1000, sessions::OTHER_COMPANY_REP)] {
        server
            .delete(&format!("/v1/customers/{customer}/payments/{id}"))
            .with_session(session)
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }
    for _ in 0..2 {
        server
            .delete(&format!("/v1/customers/1000/payments/{id}"))
            .with_session(sessions::REP)
            .await
            .assert_status(StatusCode::NO_CONTENT);
    }
    assert_eq!(status_of(&pool, id).await, "canceled");
    // Its Stripe page was closed, once.
    {
        let stripe = stripe.lock().unwrap();
        assert_eq!(stripe.count("POST", "/v1/checkout/sessions/{id}/expire"), 1);
        assert_eq!(stripe.sessions["cs_1"]["status"], "expired");
    }
    let refused = server.post(&format!("/v1/public/pay/{id}/checkout")).await;
    refused.assert_status(StatusCode::CONFLICT);
    assert_eq!(
        refused.json::<Value>()["error"],
        "This payment link is no longer active"
    );
    let page: Value = server.get(&format!("/v1/public/pay/{id}")).await.json();
    assert_eq!(page["status"], "canceled");

    // A paid one stays.
    let paid = link(&server, 1000, "deal-8000", "card", 50_000).await;
    let paid = paid["id"].as_str().unwrap();
    webhook(
        &server,
        "checkout.session.completed",
        ACCOUNT,
        session_event(paid, "paid", "pi_1"),
    )
    .await;
    server
        .delete(&format!("/v1/customers/1000/payments/{paid}"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::CONFLICT);
}

// ---- Stripe's events

#[test_db::test(seed)]
async fn the_webhook_needs_stripe_s_signature(pool: MySqlPool) {
    let (server, _) = connected_server(&pool).await;
    let payment = link(&server, 1000, "deal-8000", "card", 50_000).await;
    let id = payment["id"].as_str().unwrap();
    let body = json!({ "type": "checkout.session.completed", "account": ACCOUNT,
                       "data": { "object": session_event(id, "paid", "pi_1") } })
    .to_string();
    let now = chrono::Utc::now().timestamp();
    for signature in [
        None,
        Some(signature_header(body.as_bytes(), "whsec_other", now)),
        Some(signature_header(
            body.as_bytes(),
            WEBHOOK_SECRET,
            now - 3_600,
        )),
    ] {
        let mut request = server
            .post("/v1/webhooks/stripe-connect")
            .bytes(body.clone().into());
        if let Some(signature) = signature {
            request = request.add_header("stripe-signature", signature);
        }
        request.await.assert_status(StatusCode::BAD_REQUEST);
    }
    assert_eq!(status_of(&pool, id).await, "created");
}

#[test_db::test(seed)]
async fn a_card_payment_is_paid_at_once_and_the_company_hears(pool: MySqlPool) {
    let (server, _) = connected_server(&pool).await;
    let payment = link(&server, 1000, "deal-8000", "card", 50_050).await;
    let id = payment["id"].as_str().unwrap();

    // Another account naming this payment changes nothing.
    webhook(
        &server,
        "checkout.session.completed",
        "acct_other",
        session_event(id, "paid", "pi_x"),
    )
    .await;
    assert_eq!(status_of(&pool, id).await, "created");

    for _ in 0..2 {
        let status = webhook(
            &server,
            "checkout.session.completed",
            ACCOUNT,
            session_event(id, "paid", "pi_1"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    assert_eq!(status_of(&pool, id).await, "paid");
    // Once each: the Manager (Ada, 102) and whoever made the link (Rita, 100).
    let text = "Alice Lead paid $500.50 by card for Alice kitchen.";
    assert_eq!(
        bell(&pool).await,
        vec![
            (100, "payment_paid".to_string(), text.to_string()),
            (102, "payment_paid".to_string(), text.to_string()),
        ]
    );

    // In the bell it opens the customer's Projects tab at the payment.
    let listed: Value = server
        .get("/v1/notifications")
        .with_session(sessions::REP)
        .await
        .json();
    let row = listed["notifications"]
        .as_array()
        .or_else(|| listed.as_array())
        .and_then(|rows| {
            rows.iter()
                .find(|row| row["notification_type"] == "payment_paid")
        })
        .unwrap_or_else(|| panic!("no payment row in {listed}"));
    assert_eq!(row["message"], text);
    assert_eq!(row["type_title"], "Payment");
    assert_eq!(
        row["href"],
        format!("/employee/customers/info/1000/projects?payment={id}")
    );

    let listed: Value = server
        .get("/v1/customers/1000/payments")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(listed[0]["status"], "paid");
}

#[test_db::test(seed)]
async fn an_ach_transfer_is_pending_until_it_clears(pool: MySqlPool) {
    let (server, stripe) = connected_server(&pool).await;
    let payment = link(&server, 1004, "sale-7000", "ach", 50_000).await;
    let id = payment["id"].as_str().unwrap();
    // The finished page does not say whether the transfer started: its
    // payment intent does.
    stripe.lock().unwrap().intents.insert(
        "pi_1".into(),
        json!({ "id": "pi_1", "status": "processing" }),
    );
    webhook(
        &server,
        "checkout.session.completed",
        ACCOUNT,
        session_event(id, "unpaid", "pi_1"),
    )
    .await;
    assert_eq!(status_of(&pool, id).await, "pending");
    // The page no longer takes a payment.
    server
        .post(&format!("/v1/public/pay/{id}/checkout"))
        .await
        .assert_status(StatusCode::CONFLICT);
    webhook(
        &server,
        "checkout.session.async_payment_succeeded",
        ACCOUNT,
        session_event(id, "paid", "pi_1"),
    )
    .await;
    assert_eq!(status_of(&pool, id).await, "paid");
    let kinds: Vec<(i32, String)> = bell(&pool)
        .await
        .into_iter()
        .map(|(user, kind, _)| (user, kind))
        .collect();
    assert_eq!(
        kinds,
        vec![
            (100, "payment_pending".to_string()),
            (102, "payment_pending".to_string()),
            (100, "payment_paid".to_string()),
            (102, "payment_paid".to_string()),
        ]
    );
    assert_eq!(
        bell(&pool).await[0].2,
        "Erin Builder started an ACH payment of $500 for 5 Oak Ave. It takes about 4 business days."
    );
}

#[test_db::test(seed)]
async fn a_failed_transfer_says_why(pool: MySqlPool) {
    let (server, stripe) = connected_server(&pool).await;
    let payment = link(&server, 1004, "sale-7000", "ach", 50_000).await;
    let id = payment["id"].as_str().unwrap();
    stripe.lock().unwrap().intents.insert(
        "pi_1".into(),
        json!({ "id": "pi_1", "last_payment_error": {
            "message": "The customer's bank account has insufficient funds." } }),
    );
    webhook(
        &server,
        "checkout.session.completed",
        ACCOUNT,
        session_event(id, "unpaid", "pi_1"),
    )
    .await;
    webhook(
        &server,
        "checkout.session.async_payment_failed",
        ACCOUNT,
        session_event(id, "unpaid", "pi_1"),
    )
    .await;
    let listed: Value = server
        .get("/v1/customers/1004/payments")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(listed[0]["status"], "failed");
    assert_eq!(listed[0]["failure_reason"], "insufficient funds");
    assert_eq!(
        stripe
            .lock()
            .unwrap()
            .last("GET", "/v1/payment_intents/{id}")
            .0
            .as_deref(),
        Some(ACCOUNT)
    );
    assert_eq!(
        bell(&pool).await.last().unwrap().2,
        "Erin Builder's $500 ACH payment for 5 Oak Ave failed: insufficient funds."
    );
    // A late success does not revive it.
    webhook(
        &server,
        "checkout.session.async_payment_succeeded",
        ACCOUNT,
        session_event(id, "paid", "pi_1"),
    )
    .await;
    assert_eq!(status_of(&pool, id).await, "failed");
}

#[test_db::test(seed)]
async fn refunds_and_disputes_follow_the_charge(pool: MySqlPool) {
    let (server, _) = connected_server(&pool).await;
    let pay = |intent: &'static str| {
        let server = &server;
        async move {
            let payment = link(server, 1000, "deal-8000", "card", 50_000).await;
            let id = payment["id"].as_str().unwrap().to_string();
            webhook(
                server,
                "checkout.session.completed",
                ACCOUNT,
                session_event(&id, "paid", intent),
            )
            .await;
            id
        }
    };
    // Found by the payment intent kept when it was paid.
    let refunded = pay("pi_1").await;
    webhook(
        &server,
        "charge.refunded",
        ACCOUNT,
        json!({ "id": "ch_1", "payment_intent": "pi_1", "refunded": false }),
    )
    .await;
    assert_eq!(status_of(&pool, &refunded).await, "paid");
    webhook(
        &server,
        "charge.refunded",
        ACCOUNT,
        json!({ "id": "ch_1", "payment_intent": "pi_1", "refunded": true }),
    )
    .await;
    assert_eq!(status_of(&pool, &refunded).await, "refunded");

    let disputed = pay("pi_2").await;
    let dispute = json!({ "id": "dp_1", "payment_intent": "pi_2", "status": "needs_response" });
    webhook(&server, "charge.dispute.created", ACCOUNT, dispute).await;
    assert_eq!(status_of(&pool, &disputed).await, "disputed");
    webhook(
        &server,
        "charge.dispute.closed",
        ACCOUNT,
        json!({ "id": "dp_1", "payment_intent": "pi_2", "status": "won" }),
    )
    .await;
    assert_eq!(status_of(&pool, &disputed).await, "paid");

    let kinds: Vec<String> = bell(&pool)
        .await
        .into_iter()
        .filter(|(user, _, _)| *user == 102)
        .map(|(_, kind, _)| kind)
        .collect();
    assert_eq!(
        kinds,
        [
            "payment_paid",
            "payment_refunded",
            "payment_paid",
            "payment_disputed"
        ]
    );
}

#[test_db::test(seed)]
async fn a_declined_card_tells_the_company_and_the_link_stays_payable(pool: MySqlPool) {
    let (server, _) = connected_server(&pool).await;
    let payment = link(&server, 1000, "deal-8000", "card", 50_000).await;
    let id = payment["id"].as_str().unwrap();
    let declined = |decline_code: &str| {
        json!({ "id": "pi_1", "object": "payment_intent", "metadata": { "granite_payment": id },
                "last_payment_error": { "code": "card_declined", "decline_code": decline_code,
                                        "message": "Your card was declined." } })
    };
    // Stripe sends the same event twice: heard once.
    for _ in 0..2 {
        let status = webhook_event(
            &server,
            "evt_decline_1",
            "payment_intent.payment_failed",
            ACCOUNT,
            declined("insufficient_funds"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    assert_eq!(status_of(&pool, id).await, "created");
    let text = "Alice Lead's $500 card payment for Alice kitchen failed: insufficient funds.";
    assert_eq!(
        bell(&pool).await,
        vec![
            (100, "payment_failed".to_string(), text.to_string()),
            (102, "payment_failed".to_string(), text.to_string()),
        ]
    );
    // The customer tries another card: declined again, a new event.
    webhook(
        &server,
        "payment_intent.payment_failed",
        ACCOUNT,
        declined("generic_decline"),
    )
    .await;
    assert_eq!(
        bell(&pool).await.last().unwrap().2,
        "Alice Lead's $500 card payment for Alice kitchen failed: card declined."
    );
    // Another account's event about it is not heard.
    webhook(
        &server,
        "payment_intent.payment_failed",
        "acct_other",
        declined("generic_decline"),
    )
    .await;
    assert_eq!(bell(&pool).await.len(), 4);
    // The link still takes a payment, and the third card works.
    server
        .post(&format!("/v1/public/pay/{id}/checkout"))
        .await
        .assert_status_ok();
    webhook(
        &server,
        "payment_intent.succeeded",
        ACCOUNT,
        json!({ "id": "pi_1", "object": "payment_intent",
                "metadata": { "granite_payment": id } }),
    )
    .await;
    assert_eq!(status_of(&pool, id).await, "paid");
    // A late decline of a paid payment changes nothing.
    webhook(
        &server,
        "payment_intent.payment_failed",
        ACCOUNT,
        declined("generic_decline"),
    )
    .await;
    assert_eq!(status_of(&pool, id).await, "paid");
    assert_eq!(bell(&pool).await.len(), 6);
}

#[test_db::test(seed)]
async fn payment_intent_events_move_a_payment_like_checkout_s(pool: MySqlPool) {
    let (server, _) = connected_server(&pool).await;
    let ach = link(&server, 1004, "sale-7000", "ach", 50_000).await;
    let id = ach["id"].as_str().unwrap();
    let intent = json!({ "id": "pi_9", "object": "payment_intent",
                         "metadata": { "granite_payment": id } });
    webhook(
        &server,
        "payment_intent.processing",
        ACCOUNT,
        intent.clone(),
    )
    .await;
    assert_eq!(status_of(&pool, id).await, "pending");
    // Checkout says the same: nothing new.
    webhook(
        &server,
        "checkout.session.completed",
        ACCOUNT,
        session_event(id, "unpaid", "pi_9"),
    )
    .await;
    assert_eq!(bell(&pool).await.len(), 2);
    // The bank returned it.
    let mut returned = intent.clone();
    returned["last_payment_error"] = json!({ "code": "payment_method_insufficient_funds",
                "message": "The customer's bank account has insufficient funds." });
    webhook(&server, "payment_intent.payment_failed", ACCOUNT, returned).await;
    assert_eq!(status_of(&pool, id).await, "failed");
    assert_eq!(
        bell(&pool).await.last().unwrap().2,
        "Erin Builder's $500 ACH payment for 5 Oak Ave failed: insufficient funds."
    );
    // Checkout's word on it comes after: heard once.
    webhook(
        &server,
        "checkout.session.async_payment_failed",
        ACCOUNT,
        session_event(id, "unpaid", "pi_9"),
    )
    .await;
    assert_eq!(bell(&pool).await.len(), 4);

    // A card is never pending.
    let card = link(&server, 1004, "sale-7000", "card", 50_000).await;
    let card = card["id"].as_str().unwrap();
    let intent = json!({ "id": "pi_10", "object": "payment_intent",
                         "metadata": { "granite_payment": card } });
    webhook(
        &server,
        "payment_intent.processing",
        ACCOUNT,
        intent.clone(),
    )
    .await;
    assert_eq!(status_of(&pool, card).await, "created");
    webhook(&server, "payment_intent.succeeded", ACCOUNT, intent).await;
    assert_eq!(status_of(&pool, card).await, "paid");
    // Checkout's paid after it changes nothing.
    webhook(
        &server,
        "checkout.session.completed",
        ACCOUNT,
        session_event(card, "paid", "pi_10"),
    )
    .await;
    assert_eq!(bell(&pool).await.len(), 6);
}

#[test_db::test(seed)]
async fn a_partial_refund_keeps_the_payment_paid(pool: MySqlPool) {
    let (server, _) = connected_server(&pool).await;
    sqlx::query("UPDATE company_payment_settings SET ach_customer_share_bps = 50")
        .execute(&pool)
        .await
        .unwrap();
    // $1,000 by ACH, the customer charged $1,005.
    let payment = link(&server, 1000, "deal-8000", "ach", 100_000).await;
    let id = payment["id"].as_str().unwrap();
    webhook(
        &server,
        "checkout.session.completed",
        ACCOUNT,
        session_event(id, "paid", "pi_1"),
    )
    .await;
    let charge = |amount_refunded: i64, refunded: bool| {
        json!({ "id": "ch_1", "payment_intent": "pi_1", "amount": 100_500,
                "amount_refunded": amount_refunded, "refunded": refunded })
    };
    // $300 back, sent twice: heard once.
    for _ in 0..2 {
        webhook_event(
            &server,
            "evt_refund_1",
            "charge.refunded",
            ACCOUNT,
            charge(30_000, false),
        )
        .await;
    }
    assert_eq!(status_of(&pool, id).await, "paid");
    let listed: Value = server
        .get("/v1/customers/1000/payments")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        (
            &listed[0]["status"],
            &listed[0]["refunded_cents"],
            &listed[0]["customer_fee_bps"]
        ),
        (&json!("paid"), &json!(30_000), &json!(50))
    );
    // Another $200; an older event arriving late changes nothing.
    webhook(&server, "charge.refunded", ACCOUNT, charge(50_000, false)).await;
    webhook(&server, "charge.refunded", ACCOUNT, charge(30_000, false)).await;
    // The rest.
    webhook(&server, "charge.refunded", ACCOUNT, charge(100_500, true)).await;
    assert_eq!(status_of(&pool, id).await, "refunded");
    let refunds: Vec<String> = bell(&pool)
        .await
        .into_iter()
        .filter(|(user, kind, _)| *user == 102 && kind == "payment_refunded")
        .map(|(_, _, text)| text)
        .collect();
    assert_eq!(
        refunds,
        [
            "$300 was refunded to Alice Lead for Alice kitchen.",
            "$200 was refunded to Alice Lead for Alice kitchen.",
            "$505 was refunded to Alice Lead for Alice kitchen.",
        ]
    );
}

#[test_db::test(seed)]
async fn a_dispute_names_the_payment(pool: MySqlPool) {
    let (server, _) = connected_server(&pool).await;
    let payment = link(&server, 1000, "deal-8000", "card", 50_000).await;
    let id = payment["id"].as_str().unwrap();
    webhook(
        &server,
        "checkout.session.completed",
        ACCOUNT,
        session_event(id, "paid", "pi_1"),
    )
    .await;
    webhook(
        &server,
        "charge.dispute.created",
        ACCOUNT,
        json!({ "id": "dp_1", "payment_intent": "pi_1", "status": "needs_response" }),
    )
    .await;
    assert_eq!(
        bell(&pool).await.last().unwrap().2,
        "Alice Lead disputed the $500 payment for Alice kitchen."
    );
}

#[test_db::test(seed)]
async fn two_tabs_at_once_get_one_checkout_page(pool: MySqlPool) {
    let (server, stripe) = connected_server(&pool).await;
    let payment = link(&server, 1000, "deal-8000", "card", 50_000).await;
    let id = payment["id"].as_str().unwrap();
    let path = format!("/v1/public/pay/{id}/checkout");
    let (first, second, third) =
        tokio::join!(server.post(&path), server.post(&path), server.post(&path));
    for response in [&first, &second, &third] {
        response.assert_status_ok();
        assert_eq!(
            response.json::<Value>()["url"],
            "https://checkout.stripe.com/c/pay/cs_1"
        );
    }
    let stripe = stripe.lock().unwrap();
    assert_eq!(stripe.count("POST", "/v1/checkout/sessions"), 1);
    // A card page says which cards are taken, by Stripe's Pay button.
    let (_, form) = stripe.last("POST", "/v1/checkout/sessions");
    assert_eq!(
        form["custom_text[submit][message]"],
        "We accept cards issued in the United States only."
    );
}

#[test_db::test(seed)]
async fn a_link_paid_twice_tells_the_company_to_refund_one(pool: MySqlPool) {
    let (server, _) = connected_server(&pool).await;
    let payment = link(&server, 1000, "deal-8000", "card", 50_000).await;
    let id = payment["id"].as_str().unwrap();
    let paid = |intent: &str| json!({ "id": intent, "object": "payment_intent", "metadata": { "granite_payment": id } });
    webhook(&server, "payment_intent.succeeded", ACCOUNT, paid("pi_1")).await;
    // The same payment again (Checkout's word on it): nothing new.
    webhook(
        &server,
        "checkout.session.completed",
        ACCOUNT,
        session_event(id, "paid", "pi_1"),
    )
    .await;
    assert_eq!(bell(&pool).await.len(), 2);
    // Another payment on the same link, told twice: heard once.
    webhook(&server, "payment_intent.succeeded", ACCOUNT, paid("pi_2")).await;
    webhook(&server, "payment_intent.succeeded", ACCOUNT, paid("pi_2")).await;
    let duplicates: Vec<(i32, String, String)> = bell(&pool)
        .await
        .into_iter()
        .filter(|(_, kind, _)| kind == "payment_duplicate")
        .collect();
    let text = "Alice Lead paid $500 by card for Alice kitchen a second time. Refund the extra payment in Stripe.";
    assert_eq!(
        duplicates,
        vec![
            (100, "payment_duplicate".to_string(), text.to_string()),
            (102, "payment_duplicate".to_string(), text.to_string()),
        ]
    );
    // The first payment stays the one recorded.
    let intent: String = sqlx::query_scalar(
        "SELECT stripe_payment_intent_id FROM customer_payments WHERE public_id = ?",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (status_of(&pool, id).await, intent),
        ("paid".into(), "pi_1".into())
    );

    // The company refunds the extra charge, as asked: the payment itself
    // stays paid with nothing given back, and the company hears once.
    let extra = json!({
        "id": "ch_2", "object": "charge", "payment_intent": "pi_2",
        "refunded": true, "amount_refunded": 50_000,
        "metadata": { "granite_payment": id },
    });
    webhook(&server, "charge.refunded", ACCOUNT, extra.clone()).await;
    webhook(&server, "charge.refunded", ACCOUNT, extra).await;
    let refunded: i32 =
        sqlx::query_scalar("SELECT refunded_cents FROM customer_payments WHERE public_id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((status_of(&pool, id).await, refunded), ("paid".into(), 0));
    let heard: Vec<String> = bell(&pool)
        .await
        .into_iter()
        .filter(|(user, kind, _)| *user == 100 && kind == "payment_duplicate_refunded")
        .map(|(_, _, text)| text)
        .collect();
    assert_eq!(
        heard,
        vec![
            "The extra $500 payment for Alice kitchen was refunded to Alice Lead. The first payment stays."
                .to_string()
        ]
    );
    // Nor does a dispute of the extra charge dispute the payment.
    let dispute = json!({ "id": "dp_2", "object": "dispute", "payment_intent": "pi_2", "status": "needs_response" });
    webhook(&server, "charge.dispute.created", ACCOUNT, dispute).await;
    assert_eq!(status_of(&pool, id).await, "paid");
    // The payment's own refund still counts.
    let own = json!({
        "id": "ch_1", "object": "charge", "payment_intent": "pi_1",
        "refunded": true, "amount_refunded": 50_000,
        "metadata": { "granite_payment": id },
    });
    webhook(&server, "charge.refunded", ACCOUNT, own).await;
    assert_eq!(status_of(&pool, id).await, "refunded");
}

#[test_db::test(seed)]
async fn a_dispute_sent_again_after_it_was_won_changes_nothing(pool: MySqlPool) {
    let (server, _) = connected_server(&pool).await;
    let payment = link(&server, 1000, "deal-8000", "card", 50_000).await;
    let id = payment["id"].as_str().unwrap();
    webhook(
        &server,
        "checkout.session.completed",
        ACCOUNT,
        session_event(id, "paid", "pi_1"),
    )
    .await;
    let dispute =
        |status: &str| json!({ "id": "dp_1", "payment_intent": "pi_1", "status": status });
    webhook(
        &server,
        "charge.dispute.created",
        ACCOUNT,
        dispute("needs_response"),
    )
    .await;
    // Sent again while open: heard once.
    webhook(
        &server,
        "charge.dispute.created",
        ACCOUNT,
        dispute("needs_response"),
    )
    .await;
    assert_eq!(status_of(&pool, id).await, "disputed");
    webhook(&server, "charge.dispute.closed", ACCOUNT, dispute("won")).await;
    assert_eq!(status_of(&pool, id).await, "paid");
    // Stripe sends the dispute again after it was won.
    webhook(
        &server,
        "charge.dispute.created",
        ACCOUNT,
        dispute("needs_response"),
    )
    .await;
    assert_eq!(status_of(&pool, id).await, "paid");
    let disputes = bell(&pool)
        .await
        .into_iter()
        .filter(|(_, kind, _)| kind == "payment_disputed")
        .count();
    assert_eq!(disputes, 2, "one per user");

    // Closed (won) before Stripe's created arrives: never disputed.
    let late = link(&server, 1000, "deal-8000", "card", 50_000).await;
    let late = late["id"].as_str().unwrap();
    webhook(
        &server,
        "checkout.session.completed",
        ACCOUNT,
        session_event(late, "paid", "pi_2"),
    )
    .await;
    let dispute =
        |status: &str| json!({ "id": "dp_2", "payment_intent": "pi_2", "status": status });
    webhook(&server, "charge.dispute.closed", ACCOUNT, dispute("won")).await;
    webhook(
        &server,
        "charge.dispute.created",
        ACCOUNT,
        dispute("needs_response"),
    )
    .await;
    assert_eq!(status_of(&pool, late).await, "paid");
}

// ---- demo mode

#[test_db::test(seed)]
async fn demo_mode_never_calls_stripe(pool: MySqlPool) {
    make_manager(&pool).await;
    // A Stripe that would fail if it were called.
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_stripe(Stripe::new(Some("sk_test_fake"), "http://127.0.0.1:9"))
        .with_extension(CustomerPayments::new("", true));
    let server = TestServer::new(app(state));
    let started = server
        .post("/v1/company/payment-settings/bank-account")
        .with_session(sessions::ADMIN)
        .await;
    assert_eq!(started.json::<Value>(), json!({ "url": null }));
    let settings: Value = server
        .get("/v1/company/payment-settings")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        settings["bank"],
        json!({ "status": "connected", "bank_name": "Demo Bank", "last4": "4242" })
    );

    let payment = link(&server, 1000, "deal-8000", "ach", 50_000).await;
    let id = payment["id"].as_str().unwrap();
    let page: Value = server.get(&format!("/v1/public/pay/{id}")).await.json();
    assert_eq!(page["demo"], true);
    let checkout = server.post(&format!("/v1/public/pay/{id}/checkout")).await;
    assert_eq!(checkout.json::<Value>(), json!({ "url": null }));
    for (outcome, status) in [("pending", "pending"), ("paid", "paid"), ("failed", "paid")] {
        server
            .post(&format!("/v1/public/pay/{id}/demo"))
            .json(&json!({ "outcome": outcome }))
            .await
            .assert_status(StatusCode::NO_CONTENT);
        assert_eq!(status_of(&pool, id).await, status, "{outcome}");
    }
    assert_eq!(bell(&pool).await.len(), 4);
    server
        .delete(&format!("/v1/customers/1000/payments/{id}"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::CONFLICT);

    // A declined card is heard, and the link can still be paid.
    let card = link(&server, 1000, "deal-8000", "card", 50_000).await;
    let card = card["id"].as_str().unwrap();
    server
        .post(&format!("/v1/public/pay/{card}/demo"))
        .json(&json!({ "outcome": "failed" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(status_of(&pool, card).await, "created");
    assert_eq!(
        bell(&pool).await.last().unwrap().2,
        "Alice Lead's $500 card payment for Alice kitchen failed: card declined."
    );
}

#[test_db::test(seed)]
async fn outside_demo_mode_a_demo_account_is_not_connected(pool: MySqlPool) {
    let (server, stripe) = server(&pool).await;
    sqlx::query(
        "INSERT INTO company_payment_settings (company_id, stripe_account_id, charges_enabled)
         VALUES (100, 'acct_demo_100', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let settings: Value = server
        .get("/v1/company/payment-settings")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(settings["bank"]["status"], "not_connected");
    server
        .post("/v1/customers/1000/payments")
        .with_session(sessions::REP)
        .json(&json!({ "project_key": "deal-8000", "method": "card", "amount_cents": 50_000 }))
        .await
        .assert_status(StatusCode::CONFLICT);
    server
        .post("/v1/public/pay/pay_00000000000000000000000000000000/demo")
        .json(&json!({ "outcome": "paid" }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    assert!(stripe.lock().unwrap().requests.is_empty());
}

// ---- the customer's own page

type Sent = Arc<Mutex<Vec<OutgoingEmail>>>;

#[test_db::test(seed)]
async fn the_customer_sees_what_they_paid_and_owe(pool: MySqlPool) {
    make_manager(&pool).await;
    connect_bank(&pool).await;
    sqlx::raw_sql(
        "UPDATE customers SET view_id = UUID_TO_BIN('3b1f6c2e-9d4a-4e8b-a1c7-5f2e8d9c0b1a')
          WHERE id = 1004;
         INSERT INTO customers_emails (id, customer_id, email, created_at, created_by)
         VALUES (5900, 1004, 'erin@example.com', '2026-01-15 11:00:00', 100);
         UPDATE customers SET email_id = 5900 WHERE id = 1004;",
    )
    .execute(&pool)
    .await
    .unwrap();
    let sent = Sent::default();
    let mail = Router::new()
        .route(
            "/email/send",
            post(
                |State(sent): State<Sent>, Json(email): Json<OutgoingEmail>| async move {
                    sent.lock().unwrap().push(email);
                    Json(json!({ "message_id": "<c@ses.test>" }))
                },
            ),
        )
        .with_state(sent.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, mail).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_webhooks(WebhooksClient::new(Some(&base), Some("webhooks-key")))
        .with_extension(CustomerPayments::new(WEBHOOK_SECRET, false));
    let mut server = TestServer::new(app(state));
    server.save_cookies();
    let page = |path: &str| format!("/v1/public/customer-view/100/{VIEW}{path}");

    server
        .get(&page("/payments"))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .post(&page("/code"))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let body = sent.lock().unwrap().last().unwrap().body.clone();
    let start = body.find("margin: 24px 0;\">").unwrap() + "margin: 24px 0;\">".len();
    server
        .post(&page("/verify"))
        .json(&json!({ "code": &body[start..start + 6] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    // Erin's sales: $1,000.50 and $2,000. One link paid, one ACH on its way,
    // one open, one withdrawn; Frank's is not hers.
    let paid = link(&server, 1004, "sale-7000", "card", 100_050).await;
    let pending = link(&server, 1004, "sale-7001", "ach", 50_000).await;
    let open = link(&server, 1004, "sale-7001", "card", 25_000).await;
    let withdrawn = link(&server, 1004, "sale-7001", "card", 10_000).await;
    link(&server, 1004, "sale-7002", "card", 10_000).await;
    let id = |payment: &Value| payment["id"].as_str().unwrap().to_string();
    webhook(
        &server,
        "checkout.session.completed",
        ACCOUNT,
        session_event(&id(&paid), "paid", "pi_1"),
    )
    .await;
    webhook(
        &server,
        "checkout.session.completed",
        ACCOUNT,
        session_event(&id(&pending), "unpaid", "pi_2"),
    )
    .await;
    server
        .delete(&format!("/v1/customers/1004/payments/{}", id(&withdrawn)))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let payments = server.get(&page("/payments")).await;
    payments.assert_status_ok();
    assert_eq!(payments.header("cache-control"), "private, no-store");
    let payments: Value = payments.json();
    assert_eq!(payments["enabled"], true);
    assert_eq!(
        (
            &payments["total_cents"],
            &payments["paid_cents"],
            &payments["pending_cents"],
            &payments["balance_cents"]
        ),
        (
            &json!(420_050),
            &json!(100_050),
            &json!(50_000),
            &json!(320_000)
        )
    );
    let listed: Vec<(&str, &str)> = payments["payments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|payment| {
            (
                payment["id"].as_str().unwrap(),
                payment["status"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        listed,
        [
            (id(&open).as_str(), "created"),
            (id(&pending).as_str(), "pending"),
            (id(&paid).as_str(), "paid"),
        ]
    );
    // The company's side of the fee is not the customer's business.
    assert_eq!(
        payments["payments"][0],
        json!({ "id": id(&open), "project_key": "sale-7001", "project_name": "5 Oak Ave",
                "sale_id": 7001, "method": "card", "status": "created", "amount_cents": 25_000,
                "customer_fee_cents": 0, "refunded_cents": 0, "url": open["url"],
                "failure_reason": null,
                "created_at": open["created_at"] })
    );
    // Her jobs: the deal she signed (Erin backsplash, $1,200), then her two
    // sales. Her other deals are neither won nor sold.
    assert_eq!(
        payments["projects"],
        json!([
            { "project_key": "deal-8010", "name": "Erin backsplash", "price_cents": 120_000 },
            { "project_key": "sale-7000", "name": "5 Oak Ave", "price_cents": 100_050 },
            { "project_key": "sale-7001", "name": "5 Oak Ave", "price_cents": 200_000 },
        ])
    );

    // A deal she signed is a job too, at the deal's amount, and a sale made
    // for it is part of it. A cancelled sale is owed nothing.
    sqlx::raw_sql(
        "UPDATE deals SET is_won = 1 WHERE id = 8002;
         UPDATE sales SET deal_id = 8002 WHERE id = 7000;
         UPDATE sales SET cancelled_date = '2026-05-01 00:00:00' WHERE id = 7001;",
    )
    .execute(&pool)
    .await
    .unwrap();
    let payments: Value = server.get(&page("/payments")).await.json();
    assert_eq!(
        payments["projects"],
        json!([
            { "project_key": "deal-8010", "name": "Erin backsplash", "price_cents": 120_000 },
            { "project_key": "deal-8002", "name": "Erin countertops", "price_cents": 250_050 },
        ])
    );
    assert_eq!(
        (
            &payments["total_cents"],
            &payments["paid_cents"],
            &payments["balance_cents"]
        ),
        (&json!(370_050), &json!(100_050), &json!(270_000))
    );
    // The payment made for the sale now shows under the deal.
    let last = payments["payments"].as_array().unwrap().last().unwrap();
    assert_eq!(
        (&last["project_key"], &last["project_name"]),
        (&json!("deal-8002"), &json!("Erin countertops"))
    );
}

// ---- the company's switch

#[test_db::test(seed)]
async fn a_company_that_is_not_turned_on_sets_nothing_up(pool: MySqlPool) {
    let (server, stripe) = connected_server(&pool).await;
    // A link made while payments were on.
    let payment = link(&server, 1000, "deal-8000", "card", 50_000).await;
    let id = payment["id"].as_str().unwrap();
    sqlx::query("UPDATE company SET customer_payments_enabled = 0 WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();

    let settings = server
        .get("/v1/company/payment-settings")
        .with_session(sessions::ADMIN)
        .await;
    settings.assert_status_ok();
    assert_eq!(settings.json::<Value>()["enabled"], false);
    let refused = "Customer payments are not turned on for this company";
    let fees = server
        .put("/v1/company/payment-settings")
        .with_session(sessions::ADMIN)
        .json(&json!({ "card_customer_share_bps": 0, "ach_customer_share_bps": 40 }))
        .await;
    fees.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(fees.json::<Value>()["error"], refused);
    server
        .post("/v1/company/payment-settings/bank-account")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let made = server
        .post("/v1/customers/1000/payments")
        .with_session(sessions::REP)
        .json(&json!({ "project_key": "deal-8000", "method": "card", "amount_cents": 1_000 }))
        .await;
    made.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(made.json::<Value>()["error"], refused);
    assert_eq!(stripe.lock().unwrap().count("POST", "/v2/core/accounts"), 0);

    // The customer's money must not be stuck: the old link is still paid.
    server
        .get(&format!("/v1/public/pay/{id}"))
        .await
        .assert_status_ok();
    server
        .post(&format!("/v1/public/pay/{id}/checkout"))
        .await
        .assert_status_ok();
    webhook(
        &server,
        "checkout.session.completed",
        ACCOUNT,
        session_event(id, "paid", "pi_1"),
    )
    .await;
    assert_eq!(status_of(&pool, id).await, "paid");
}

// ---- a webhook that never came

/// Lets Stripe be asked again about `id` at once.
async fn forget_the_check(pool: &MySqlPool, id: &str) {
    sqlx::query(
        "UPDATE customer_payments
            SET stripe_checked_at = UTC_TIMESTAMP() - INTERVAL 1 HOUR WHERE public_id = ?",
    )
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
}

#[test_db::test(seed)]
async fn the_page_asks_stripe_when_the_webhook_did_not_come(pool: MySqlPool) {
    let (server, stripe) = connected_server(&pool).await;
    let payment = link(&server, 1000, "deal-8000", "card", 50_000).await;
    let id = payment["id"].as_str().unwrap();
    let page = format!("/v1/public/pay/{id}");
    let asked = |stripe: &Shared| {
        stripe
            .lock()
            .unwrap()
            .count("GET", "/v1/checkout/sessions/{id}")
    };

    // Before the customer went to Stripe there is nothing to ask about.
    assert_eq!(server.get(&page).await.json::<Value>()["status"], "created");
    assert_eq!(asked(&stripe), 0);

    server
        .post(&format!("{page}/checkout"))
        .await
        .assert_status_ok();
    // Stripe's page is open and unpaid: asked once, then not for a minute.
    assert_eq!(server.get(&page).await.json::<Value>()["status"], "created");
    assert_eq!(server.get(&page).await.json::<Value>()["status"], "created");
    assert_eq!(asked(&stripe), 1);

    // The customer paid and no webhook came.
    {
        let mut stripe = stripe.lock().unwrap();
        let session = stripe.sessions.get_mut("cs_1").unwrap();
        session["status"] = json!("complete");
        session["payment_status"] = json!("paid");
        session["payment_intent"] = json!("pi_1");
    }
    // Within the minute the page still shows what is kept.
    assert_eq!(server.get(&page).await.json::<Value>()["status"], "created");
    forget_the_check(&pool, id).await;
    assert_eq!(server.get(&page).await.json::<Value>()["status"], "paid");
    let intent: String = sqlx::query_scalar(
        "SELECT stripe_payment_intent_id FROM customer_payments WHERE public_id = ?",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(intent, "pi_1");
    // The company heard, once: the webhook coming late adds nothing, and a
    // paid payment is not asked about again.
    let heard = bell(&pool).await.len();
    assert_eq!(heard, 2);
    webhook(
        &server,
        "checkout.session.completed",
        ACCOUNT,
        session_event(id, "paid", "pi_1"),
    )
    .await;
    assert_eq!(bell(&pool).await.len(), heard);
    let before = asked(&stripe);
    forget_the_check(&pool, id).await;
    server.get(&page).await.assert_status_ok();
    assert_eq!(asked(&stripe), before);
}

#[test_db::test(seed)]
async fn the_list_asks_stripe_about_a_transfer_on_its_way(pool: MySqlPool) {
    let (server, stripe) = connected_server(&pool).await;
    let payment = link(&server, 1000, "deal-8000", "ach", 50_000).await;
    let id = payment["id"].as_str().unwrap();
    server
        .post(&format!("/v1/public/pay/{id}/checkout"))
        .await
        .assert_status_ok();
    let set = |status: &str, error: Value| {
        let mut stripe = stripe.lock().unwrap();
        let session = stripe.sessions.get_mut("cs_1").unwrap();
        session["status"] = json!("complete");
        session["payment_status"] = json!("unpaid");
        session["payment_intent"] = json!("pi_1");
        stripe.intents.insert(
            "pi_1".into(),
            json!({ "id": "pi_1", "status": status, "last_payment_error": error }),
        );
    };
    let listed = || async {
        let list = server
            .get("/v1/customers/1000/payments")
            .with_session(sessions::REP)
            .await;
        list.assert_status_ok();
        list.json::<Value>()[0]["status"].clone()
    };

    // The transfer started and no webhook came.
    set("processing", Value::Null);
    assert_eq!(listed().await, "pending");
    assert_eq!(bell(&pool).await.last().unwrap().1, "payment_pending");
    // It bounced: asked again only after a while.
    set(
        "requires_payment_method",
        json!({ "code": "payment_intent_payment_attempt_failed",
                "decline_code": "insufficient_funds", "message": "Insufficient funds." }),
    );
    assert_eq!(listed().await, "pending");
    forget_the_check(&pool, id).await;
    assert_eq!(listed().await, "failed");
    let reason: Option<String> =
        sqlx::query_scalar("SELECT failure_reason FROM customer_payments WHERE public_id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(reason.as_deref(), Some("insufficient funds"));
}

#[test_db::test(seed)]
async fn a_transfer_that_bounced_unheard_fails_the_link(pool: MySqlPool) {
    let (server, stripe) = connected_server(&pool).await;
    let payment = link(&server, 1000, "deal-8000", "ach", 50_000).await;
    let id = payment["id"].as_str().unwrap();
    let page = format!("/v1/public/pay/{id}");
    server
        .post(&format!("{page}/checkout"))
        .await
        .assert_status_ok();
    // Neither the start of the transfer nor its bounce was heard.
    {
        let mut stripe = stripe.lock().unwrap();
        let session = stripe.sessions.get_mut("cs_1").unwrap();
        session["status"] = json!("complete");
        session["payment_status"] = json!("unpaid");
        session["payment_intent"] = json!("pi_1");
        stripe.intents.insert(
            "pi_1".into(),
            json!({ "id": "pi_1", "status": "requires_payment_method",
                    "last_payment_error": { "decline_code": "insufficient_funds" } }),
        );
    }
    assert_eq!(server.get(&page).await.json::<Value>()["status"], "failed");
    assert_eq!(bell(&pool).await.last().unwrap().1, "payment_failed");
}

#[test_db::test(seed)]
async fn stripe_not_answering_leaves_the_page_as_it_is(pool: MySqlPool) {
    let (server, stripe) = connected_server(&pool).await;
    let payment = link(&server, 1000, "deal-8000", "card", 50_000).await;
    let id = payment["id"].as_str().unwrap();
    server
        .post(&format!("/v1/public/pay/{id}/checkout"))
        .await
        .assert_status_ok();
    // Stripe no longer knows the session (its answer is an error).
    stripe.lock().unwrap().sessions.clear();
    let page = server.get(&format!("/v1/public/pay/{id}")).await;
    page.assert_status_ok();
    assert_eq!(page.json::<Value>()["status"], "created");
}

// ---- what the review of 2026-11-20 found

/// The customer typed their bank account by hand: Stripe's page finishes,
/// and nothing is debited until they confirm two small deposits.
#[test_db::test(seed)]
async fn a_bank_account_not_confirmed_yet_is_not_a_transfer_on_its_way(pool: MySqlPool) {
    let (server, stripe) = connected_server(&pool).await;
    let payment = link(&server, 1004, "sale-7000", "ach", 50_000).await;
    let id = payment["id"].as_str().unwrap();
    let page = format!("/v1/public/pay/{id}");
    server
        .post(&format!("{page}/checkout"))
        .await
        .assert_status_ok();
    {
        let mut stripe = stripe.lock().unwrap();
        stripe.intents.insert(
            "pi_1".into(),
            json!({ "id": "pi_1", "status": "requires_action" }),
        );
        let session = stripe.sessions.get_mut("cs_1").unwrap();
        session["status"] = json!("complete");
        session["payment_status"] = json!("unpaid");
        session["payment_intent"] = json!("pi_1");
    }
    webhook(
        &server,
        "checkout.session.completed",
        ACCOUNT,
        session_event(id, "unpaid", "pi_1"),
    )
    .await;
    assert_eq!(status_of(&pool, id).await, "created");
    // Asking Stripe from the page says the same.
    forget_the_check(&pool, id).await;
    assert_eq!(server.get(&page).await.json::<Value>()["status"], "created");
    assert_eq!(bell(&pool).await, vec![]);
    // The page that was finished takes no second payment.
    let again = server.post(&format!("{page}/checkout")).await;
    again.assert_status(StatusCode::CONFLICT);
    assert_eq!(
        again.json::<Value>()["error"],
        "This payment is already on its way"
    );

    // Confirmed: the transfer starts.
    webhook(
        &server,
        "payment_intent.processing",
        ACCOUNT,
        json!({ "id": "pi_1", "status": "processing",
                "metadata": { "granite_payment": id } }),
    )
    .await;
    assert_eq!(status_of(&pool, id).await, "pending");
}

/// Stripe calls a payment intent off: a transfer held to be on its way
/// fails, a link nobody paid stays payable.
#[test_db::test(seed)]
async fn a_canceled_intent_fails_only_a_transfer_on_its_way(pool: MySqlPool) {
    let (server, _) = connected_server(&pool).await;
    let canceled = |id: &str, intent: &str| {
        json!({ "id": intent, "status": "canceled", "last_payment_error": null,
                "metadata": { "granite_payment": id } })
    };
    let waiting = link(&server, 1004, "sale-7000", "ach", 50_000).await;
    let waiting = waiting["id"].as_str().unwrap();
    webhook(
        &server,
        "payment_intent.canceled",
        ACCOUNT,
        canceled(waiting, "pi_1"),
    )
    .await;
    assert_eq!(status_of(&pool, waiting).await, "created");
    assert_eq!(bell(&pool).await, vec![]);

    sqlx::query("UPDATE customer_payments SET status = 'pending' WHERE public_id = ?")
        .bind(waiting)
        .execute(&pool)
        .await
        .unwrap();
    webhook(
        &server,
        "payment_intent.canceled",
        ACCOUNT,
        canceled(waiting, "pi_1"),
    )
    .await;
    let listed: Value = server
        .get("/v1/customers/1004/payments")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(listed[0]["status"], "failed");
    assert_eq!(
        listed[0]["failure_reason"],
        "the transfer was canceled before it started"
    );
    // The amount is free for a new link.
    link(&server, 1004, "sale-7000", "ach", 100_050).await;
}

/// Stripe keeps no order: the refund of a payment can be told before the
/// payment itself.
#[test_db::test(seed)]
async fn a_refund_told_before_the_payment_is_kept(pool: MySqlPool) {
    let (server, _) = connected_server(&pool).await;
    let payment = link(&server, 1000, "deal-8000", "card", 50_000).await;
    let id = payment["id"].as_str().unwrap();
    let charge = |refunded: i64, full: bool| {
        json!({ "id": "ch_1", "payment_intent": "pi_1", "refunded": full, "status": "succeeded",
                "amount_refunded": refunded, "metadata": { "granite_payment": id } })
    };
    // A charge that has not cleared is not a payment made.
    let mut unsettled = charge(20_000, false);
    unsettled["status"] = json!("pending");
    webhook(&server, "charge.refunded", ACCOUNT, unsettled).await;
    assert_eq!(status_of(&pool, id).await, "created");
    webhook(&server, "charge.refunded", ACCOUNT, charge(20_000, false)).await;
    let kept: (String, i32, Option<String>) = sqlx::query_as(
        "SELECT CAST(status AS CHAR), refunded_cents, stripe_payment_intent_id
           FROM customer_payments WHERE public_id = ?",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kept, ("paid".into(), 20_000, Some("pi_1".into())));
    // The event that says it was paid comes late and changes nothing.
    webhook(
        &server,
        "checkout.session.completed",
        ACCOUNT,
        session_event(id, "paid", "pi_1"),
    )
    .await;
    webhook(&server, "charge.refunded", ACCOUNT, charge(50_000, true)).await;
    assert_eq!(status_of(&pool, id).await, "refunded");
    let kinds: Vec<String> = bell(&pool)
        .await
        .into_iter()
        .filter(|(user, _, _)| *user == 102)
        .map(|(_, kind, _)| kind)
        .collect();
    assert_eq!(
        kinds,
        ["payment_paid", "payment_refunded", "payment_refunded"]
    );
    // Nothing of it counts toward the $1,000 job any more.
    link(&server, 1000, "deal-8000", "card", 100_000).await;
}

/// A disputed payment still covers the job: the company holds the money
/// until the bank decides. Lost, the money is gone and the job is owed again.
#[test_db::test(seed)]
async fn a_disputed_payment_covers_the_job_until_the_bank_decides(pool: MySqlPool) {
    let (server, _) = connected_server(&pool).await;
    let payment = link(&server, 1000, "deal-8000", "card", 100_000).await;
    let id = payment["id"].as_str().unwrap();
    webhook(
        &server,
        "checkout.session.completed",
        ACCOUNT,
        session_event(id, "paid", "pi_1"),
    )
    .await;
    let dispute =
        |status: &str| json!({ "id": "dp_1", "payment_intent": "pi_1", "status": status });
    webhook(
        &server,
        "charge.dispute.created",
        ACCOUNT,
        dispute("needs_response"),
    )
    .await;
    assert_eq!(status_of(&pool, id).await, "disputed");
    let more = || {
        server
            .post("/v1/customers/1000/payments")
            .with_session(sessions::REP)
            .json(&json!({ "project_key": "deal-8000", "method": "card", "amount_cents": 100_000 }))
    };
    let refused = more().await;
    refused.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(refused.json::<Value>()["error"], "The open balance is $0");

    for _ in 0..2 {
        webhook(&server, "charge.dispute.closed", ACCOUNT, dispute("lost")).await;
    }
    let listed: Value = server
        .get("/v1/customers/1000/payments")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(listed[0]["status"], "lost");
    let lost: Vec<String> = bell(&pool)
        .await
        .into_iter()
        .filter(|(user, kind, _)| *user == 102 && kind == "payment_dispute_lost")
        .map(|(_, _, text)| text)
        .collect();
    assert_eq!(
        lost,
        [
            "The dispute of the $1,000 payment for Alice kitchen was lost: the bank gave the money back to Alice Lead."
        ]
    );
    more().await.assert_status(StatusCode::CREATED);
}

/// The dispute's close can be told before its opening.
#[test_db::test(seed)]
async fn a_dispute_lost_before_it_was_heard_of_is_still_lost(pool: MySqlPool) {
    let (server, _) = connected_server(&pool).await;
    let payment = link(&server, 1000, "deal-8000", "card", 50_000).await;
    let id = payment["id"].as_str().unwrap();
    let dispute = |status: &str| {
        json!({ "id": "dp_1", "payment_intent": "pi_1", "status": status,
                "metadata": { "granite_payment": id } })
    };
    webhook(&server, "charge.dispute.closed", ACCOUNT, dispute("lost")).await;
    assert_eq!(status_of(&pool, id).await, "lost");
    webhook(
        &server,
        "charge.dispute.created",
        ACCOUNT,
        dispute("needs_response"),
    )
    .await;
    assert_eq!(status_of(&pool, id).await, "lost");
}

/// Sale 7000 ($1,000.50) is paid for and then joins deal 8002 ($2,500.50):
/// what was paid counts toward the deal, and the sale takes no link of its
/// own any more.
#[test_db::test(seed)]
async fn a_deal_s_balance_counts_what_its_sales_were_paid(pool: MySqlPool) {
    let (server, _) = connected_server(&pool).await;
    let payment = link(&server, 1004, "sale-7000", "card", 100_050).await;
    webhook(
        &server,
        "checkout.session.completed",
        ACCOUNT,
        session_event(payment["id"].as_str().unwrap(), "paid", "pi_1"),
    )
    .await;
    sqlx::query("UPDATE sales SET deal_id = 8002 WHERE id = 7000")
        .execute(&pool)
        .await
        .unwrap();
    let ask = |key: &'static str, cents: i64| {
        server
            .post("/v1/customers/1004/payments")
            .with_session(sessions::REP)
            .json(&json!({ "project_key": key, "method": "card", "amount_cents": cents }))
    };
    let refused = ask("deal-8002", 150_001).await;
    refused.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        refused.json::<Value>()["error"],
        "The open balance is $1,500"
    );
    let refused = ask("sale-7000", 10_000).await;
    refused.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        refused.json::<Value>()["error"],
        "This sale is part of project #8002: make the link for the project"
    );
    ask("deal-8002", 150_000)
        .await
        .assert_status(StatusCode::CREATED);

    // A cancelled sale is not owed.
    sqlx::query("UPDATE sales SET cancelled_date = NOW() WHERE id = 7001")
        .execute(&pool)
        .await
        .unwrap();
    let refused = ask("sale-7001", 10_000).await;
    refused.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        refused.json::<Value>()["error"],
        "This sale was cancelled, so there is nothing to pay for it"
    );
}

// ---- the company's Stripe account

/// The company moves to another Stripe account. A link charges the account
/// it was made for, so an unpaid link of the old account is withdrawn rather
/// than paid into it; what was paid or is on its way there keeps following
/// the old account's events, and new links charge the new account.
#[test_db::test(seed)]
async fn a_link_stays_with_the_stripe_account_it_was_made_for(pool: MySqlPool) {
    let (server, stripe) = connected_server(&pool).await;
    let id = |payment: &Value| payment["id"].as_str().unwrap().to_string();
    // Paid on the old account.
    let paid = id(&link(&server, 1000, "deal-8000", "card", 30_000).await);
    webhook(
        &server,
        "checkout.session.completed",
        ACCOUNT,
        session_event(&paid, "paid", "pi_1"),
    )
    .await;
    // Not paid, with Stripe's page open on the old account.
    let open = id(&link(&server, 1000, "deal-8000", "card", 70_000).await);
    server
        .post(&format!("/v1/public/pay/{open}/checkout"))
        .await
        .assert_status_ok();
    // A transfer on its way on the old account.
    let moving = id(&link(&server, 1004, "sale-7000", "ach", 50_000).await);
    let intent = |status: &str| json!({ "id": "pi_2", "status": status, "metadata": { "granite_payment": &moving } });
    webhook(
        &server,
        "payment_intent.processing",
        ACCOUNT,
        intent("processing"),
    )
    .await;
    assert_eq!(status_of(&pool, &moving).await, "pending");

    sqlx::query("UPDATE company_payment_settings SET stripe_account_id = 'acct_2'")
        .execute(&pool)
        .await
        .unwrap();

    // The CRM's list withdraws the old account's unpaid link and closes its
    // page at Stripe, on the old account.
    let listed: Value = server
        .get("/v1/customers/1000/payments")
        .with_session(sessions::REP)
        .await
        .json();
    let status_in_list = |listed: &Value, id: &str| {
        listed
            .as_array()
            .unwrap()
            .iter()
            .find(|payment| payment["id"] == id)
            .unwrap()["status"]
            .clone()
    };
    assert_eq!(status_in_list(&listed, &open), "canceled");
    assert_eq!(status_in_list(&listed, &paid), "paid");
    {
        let stripe = stripe.lock().unwrap();
        assert_eq!(stripe.count("POST", "/v1/checkout/sessions/{id}/expire"), 1);
        assert_eq!(
            stripe
                .last("POST", "/v1/checkout/sessions/{id}/expire")
                .0
                .as_deref(),
            Some(ACCOUNT)
        );
    }
    // Its link no longer takes a payment, on either account.
    assert_eq!(
        server
            .get(&format!("/v1/public/pay/{open}"))
            .await
            .json::<Value>()["status"],
        "canceled"
    );
    let refused = server
        .post(&format!("/v1/public/pay/{open}/checkout"))
        .await;
    refused.assert_status(StatusCode::CONFLICT);
    assert_eq!(
        refused.json::<Value>()["error"],
        "This payment link is no longer active"
    );
    assert_eq!(
        stripe
            .lock()
            .unwrap()
            .count("POST", "/v1/checkout/sessions"),
        1
    );

    // The $700 it asked for is free again, and the new link charges the new
    // account.
    let fresh = id(&link(&server, 1000, "deal-8000", "card", 70_000).await);
    server
        .post(&format!("/v1/public/pay/{fresh}/checkout"))
        .await
        .assert_status_ok();
    assert_eq!(
        stripe
            .lock()
            .unwrap()
            .last("POST", "/v1/checkout/sessions")
            .0
            .as_deref(),
        Some("acct_2")
    );
    webhook(
        &server,
        "checkout.session.completed",
        "acct_2",
        session_event(&fresh, "paid", "pi_9"),
    )
    .await;
    assert_eq!(status_of(&pool, &fresh).await, "paid");

    // The old account's events still move what was paid or started there.
    webhook(
        &server,
        "charge.refunded",
        ACCOUNT,
        json!({ "id": "ch_1", "payment_intent": "pi_1", "refunded": true,
                "amount_refunded": 30_000, "status": "succeeded" }),
    )
    .await;
    assert_eq!(status_of(&pool, &paid).await, "refunded");
    webhook(
        &server,
        "payment_intent.succeeded",
        ACCOUNT,
        intent("succeeded"),
    )
    .await;
    assert_eq!(status_of(&pool, &moving).await, "paid");
    // The new account cannot touch a payment of the old one.
    webhook(
        &server,
        "checkout.session.completed",
        "acct_2",
        session_event(&open, "paid", "pi_8"),
    )
    .await;
    assert_eq!(status_of(&pool, &open).await, "canceled");
    // Money that still arrives on the page left open at the old account is
    // recorded: it is the company's.
    webhook(
        &server,
        "checkout.session.completed",
        ACCOUNT,
        session_event(&open, "paid", "pi_3"),
    )
    .await;
    assert_eq!(status_of(&pool, &open).await, "paid");
}

/// Without a Stripe account at all (it was removed), nothing is payable.
#[test_db::test(seed)]
async fn a_link_of_a_removed_stripe_account_is_withdrawn(pool: MySqlPool) {
    let (server, stripe) = connected_server(&pool).await;
    let payment = link(&server, 1000, "deal-8000", "card", 30_000).await;
    let id = payment["id"].as_str().unwrap();
    sqlx::query(
        "UPDATE company_payment_settings SET stripe_account_id = NULL, charges_enabled = 0",
    )
    .execute(&pool)
    .await
    .unwrap();
    server
        .post(&format!("/v1/public/pay/{id}/checkout"))
        .await
        .assert_status(StatusCode::CONFLICT);
    assert_eq!(status_of(&pool, id).await, "canceled");
    assert_eq!(
        stripe
            .lock()
            .unwrap()
            .count("POST", "/v1/checkout/sessions"),
        0
    );

    // How a company moves to another Stripe account: its old one is removed
    // (by support), then "Add bank account" makes a new one, which starts
    // unverified and without the old account's bank.
    stripe
        .lock()
        .unwrap()
        .accounts
        .insert(ACCOUNT.into(), json!({ "id": ACCOUNT }));
    server
        .post("/v1/company/payment-settings/bank-account")
        .with_session(sessions::ADMIN)
        .await
        .assert_status_ok();
    let kept: (Option<String>, bool, Option<String>) = sqlx::query_as(
        "SELECT stripe_account_id, charges_enabled <> 0, bank_name
           FROM company_payment_settings WHERE company_id = 100",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kept, (Some("acct_2".into()), false, None));
    let new_link = || {
        server
            .post("/v1/customers/1000/payments")
            .with_session(sessions::REP)
            .json(&json!({ "project_key": "deal-8000", "method": "card", "amount_cents": 30_000 }))
    };
    // No link until Stripe approved the new account.
    new_link().await.assert_status(StatusCode::CONFLICT);
    webhook(
        &server,
        "account.updated",
        "acct_2",
        json!({ "id": "acct_2", "charges_enabled": true, "external_accounts": { "data": [
            { "bank_name": "New Bank", "last4": "2222", "default_for_currency": true },
        ] } }),
    )
    .await;
    let settings: Value = server
        .get("/v1/company/payment-settings")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        settings["bank"],
        json!({ "status": "connected", "bank_name": "New Bank", "last4": "2222" })
    );
    // The old account saying something no longer changes the company.
    webhook(
        &server,
        "account.updated",
        ACCOUNT,
        json!({ "id": ACCOUNT, "charges_enabled": false }),
    )
    .await;
    let made = new_link().await;
    made.assert_status(StatusCode::CREATED);
    let fresh = made.json::<Value>()["id"].as_str().unwrap().to_string();
    server
        .post(&format!("/v1/public/pay/{fresh}/checkout"))
        .await
        .assert_status_ok();
    assert_eq!(
        stripe
            .lock()
            .unwrap()
            .last("POST", "/v1/checkout/sessions")
            .0
            .as_deref(),
        Some("acct_2")
    );
    // The old link stays withdrawn.
    assert_eq!(status_of(&pool, id).await, "canceled");
}

/// A dispute's own status says where it is, whichever event carries it.
#[test_db::test(seed)]
async fn a_dispute_s_own_status_decides(pool: MySqlPool) {
    let (server, _) = connected_server(&pool).await;
    let pay = |key: &'static str, method: &'static str, intent: &'static str| {
        let server = &server;
        async move {
            let customer = if key.starts_with("sale") { 1004 } else { 1000 };
            let payment = link(server, customer, key, method, 20_000).await;
            let id = payment["id"].as_str().unwrap().to_string();
            webhook(
                server,
                "payment_intent.succeeded",
                ACCOUNT,
                json!({ "id": intent, "status": "succeeded",
                        "metadata": { "granite_payment": &id } }),
            )
            .await;
            id
        }
    };
    let dispute = |id: &str, intent: &str, status: &str| json!({ "id": id, "payment_intent": intent, "status": status });

    // A bank transfer's dispute is final: it can arrive already lost.
    let transfer = pay("sale-7000", "ach", "pi_1").await;
    webhook(
        &server,
        "charge.dispute.created",
        ACCOUNT,
        dispute("dp_1", "pi_1", "lost"),
    )
    .await;
    assert_eq!(status_of(&pool, &transfer).await, "lost");
    let kinds: Vec<String> = bell(&pool)
        .await
        .into_iter()
        .filter(|(user, _, _)| *user == 102)
        .map(|(_, kind, _)| kind)
        .collect();
    assert_eq!(kinds, ["payment_paid", "payment_dispute_lost"]);

    // An inquiry the bank dropped: paid again.
    let asked = pay("deal-8000", "card", "pi_2").await;
    webhook(
        &server,
        "charge.dispute.created",
        ACCOUNT,
        dispute("dp_2", "pi_2", "warning_needs_response"),
    )
    .await;
    assert_eq!(status_of(&pool, &asked).await, "disputed");
    webhook(
        &server,
        "charge.dispute.closed",
        ACCOUNT,
        dispute("dp_2", "pi_2", "warning_closed"),
    )
    .await;
    assert_eq!(status_of(&pool, &asked).await, "paid");

    // The opening was never told: any later event of the dispute opens it,
    // once.
    let late = pay("deal-8000", "card", "pi_3").await;
    for _ in 0..2 {
        webhook(
            &server,
            "charge.dispute.funds_withdrawn",
            ACCOUNT,
            dispute("dp_3", "pi_3", "needs_response"),
        )
        .await;
    }
    assert_eq!(status_of(&pool, &late).await, "disputed");
    let disputed = bell(&pool)
        .await
        .into_iter()
        .filter(|(user, kind, _)| *user == 102 && kind == "payment_disputed")
        .count();
    assert_eq!(disputed, 2);
}

/// The bank shown is the one payouts go to, and a company that cuts the
/// platform off from its Stripe account is no longer connected.
#[test_db::test(seed)]
async fn the_company_s_account_follows_what_stripe_says(pool: MySqlPool) {
    let (server, _) = connected_server(&pool).await;
    webhook(
        &server,
        "account.updated",
        ACCOUNT,
        json!({ "id": ACCOUNT, "charges_enabled": true, "external_accounts": { "data": [
            { "bank_name": "Old Bank", "last4": "1111", "default_for_currency": false },
            { "bank_name": "New Bank", "last4": "2222", "default_for_currency": true },
        ] } }),
    )
    .await;
    let settings = || async {
        server
            .get("/v1/company/payment-settings")
            .with_session(sessions::REP)
            .await
            .json::<Value>()
    };
    assert_eq!(
        settings().await["bank"],
        json!({ "status": "connected", "bank_name": "New Bank", "last4": "2222" })
    );

    let status = webhook(
        &server,
        "account.application.deauthorized",
        ACCOUNT,
        json!({ "id": "ca_1", "object": "application" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(settings().await["bank"]["status"], "pending_verification");
    server
        .post("/v1/customers/1000/payments")
        .with_session(sessions::REP)
        .json(&json!({ "project_key": "deal-8000", "method": "card", "amount_cents": 20_000 }))
        .await
        .assert_status(StatusCode::CONFLICT);
}
