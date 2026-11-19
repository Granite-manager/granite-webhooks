//! Sign-in steps (`routes/login_steps.rs`) and the user's two-step sign-in
//! (`routes/me/mfa.rs`): the password 1111 never signs in, an
//! authenticator code after the password, superusers must set one up.

use crate::common;

use api::app;
use api::state::AppState;
use api_accounts::mfa::{base32_decode, code_at};
use axum::http::StatusCode;
use axum_test::{TestResponse, TestServer};
use common::{ALLOWED_ORIGIN, SECRET};
use serde_json::{Value, json};
use sqlx::MySqlPool;

const PASSWORD: &str = "granite-local";

fn server(pool: MySqlPool) -> TestServer {
    TestServer::new(app(AppState::new(pool, SECRET, ALLOWED_ORIGIN)))
}

/// `name=value` of the session cookie a response set.
fn session_cookie(response: &TestResponse) -> Option<String> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .rfind(|v| v.starts_with("__session=") && !v.starts_with("__session=;"))
        .map(|set| set.split(';').next().unwrap().to_string())
}

/// The code of `secret` (base32) `offset` steps from now.
fn code(secret: &str, offset: i64) -> String {
    let key = base32_decode(secret).unwrap();
    let step = chrono::Utc::now().timestamp().div_euclid(30) + offset;
    format!("{:06}", code_at(&key, step))
}

async fn set_password(pool: &MySqlPool, user_id: i32, password: &str) {
    let hash = bcrypt::hash(password, 4).unwrap();
    sqlx::query("UPDATE users SET password = ? WHERE id = ?")
        .bind(hash)
        .bind(user_id)
        .execute(pool)
        .await
        .unwrap();
}

async fn make_superuser(pool: &MySqlPool, user_id: i32) {
    sqlx::query("UPDATE users SET is_superuser = 1 WHERE id = ?")
        .bind(user_id)
        .execute(pool)
        .await
        .unwrap();
}

async fn login(server: &TestServer, email: &str, password: &str) -> TestResponse {
    server
        .post("/v1/auth/login")
        .json(&json!({ "email": email, "password": password }))
        .await
}

/// Turns two-step sign-in on for the cookie's user; the secret.
async fn enable(server: &TestServer, cookie: &str) -> String {
    let setup: Value = server
        .post("/v1/me/mfa/setup")
        .add_header("cookie", cookie.to_string())
        .await
        .json();
    let secret = setup["secret"].as_str().unwrap().to_string();
    server
        .post("/v1/me/mfa/confirm")
        .add_header("cookie", cookie.to_string())
        .json(&json!({ "code": code(&secret, 0) }))
        .await
        .assert_status_ok();
    secret
}

#[test_db::test(seed)]
async fn the_password_1111_never_signs_in(pool: MySqlPool) {
    set_password(&pool, 100, "1111").await;
    let server = server(pool.clone());
    let refused = login(&server, "rep@acme.test", "1111").await;
    refused.assert_status_forbidden();
    assert!(session_cookie(&refused).is_none());
    assert!(
        refused.json::<Value>()["error"]
            .as_str()
            .unwrap()
            .contains("Forgot password")
    );
    let app = server
        .post("/v1/auth/device-sessions")
        .json(&json!({
            "email": "rep@acme.test",
            "password": "1111",
            "deviceName": "Rita's Pixel",
            "platform": "android",
            "appVersion": "1.9.0",
            "steps": true,
        }))
        .await;
    app.assert_status_forbidden();
    assert!(app.json::<Value>().get("token").is_none());
    let challenges: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM login_challenges")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(challenges, 0);

    // With two-step sign-in on too: refused before any step.
    let cookie = {
        set_password(&pool, 100, PASSWORD).await;
        session_cookie(&login(&server, "rep@acme.test", PASSWORD).await).unwrap()
    };
    enable(&server, &cookie).await;
    set_password(&pool, 100, "1111").await;
    login(&server, "rep@acme.test", "1111")
        .await
        .assert_status_forbidden();
}

#[test_db::test(seed)]
async fn a_code_is_asked_after_the_password(pool: MySqlPool) {
    let server = server(pool.clone());
    let cookie = session_cookie(&login(&server, "rep@acme.test", PASSWORD).await).unwrap();
    let status: Value = server
        .get("/v1/me/mfa")
        .add_header("cookie", cookie.clone())
        .await
        .json();
    assert_eq!(status["enabled"], false);
    assert_eq!(status["required"], false);
    let secret = enable(&server, &cookie).await;
    // The session that turned it on stays signed in.
    let status: Value = server
        .get("/v1/me/mfa")
        .add_header("cookie", cookie.clone())
        .await
        .json();
    assert_eq!(status["enabled"], true);

    let challenge: Value = login(&server, "rep@acme.test", PASSWORD).await.json();
    assert_eq!(challenge["step"], "code");
    let token = challenge["token"].as_str().unwrap();
    let wrong = server
        .post("/v1/auth/login/code")
        .json(&json!({ "token": token, "code": "000000" }))
        .await;
    wrong.assert_status_bad_request();
    // The code confirm spent is not good again; the next step's is.
    server
        .post("/v1/auth/login/code")
        .json(&json!({ "token": token, "code": code(&secret, 0) }))
        .await
        .assert_status_bad_request();
    let done = server
        .post("/v1/auth/login/code")
        .json(&json!({ "token": token, "code": code(&secret, 1) }))
        .await;
    done.assert_status_ok();
    assert!(session_cookie(&done).is_some());
    // Wrong codes counted as failed sign-ins until it signed in.
    let failures: i64 = sqlx::query_scalar("SELECT CAST(COUNT(*) AS SIGNED) FROM login_attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(failures, 0);

    // Off with a current code.
    sqlx::query("UPDATE user_mfa SET last_step = NULL WHERE user_id = 100")
        .execute(&pool)
        .await
        .unwrap();
    server
        .post("/v1/me/mfa/disable")
        .add_header("cookie", cookie.clone())
        .json(&json!({ "code": "000000" }))
        .await
        .assert_status_bad_request();
    let off: Value = server
        .post("/v1/me/mfa/disable")
        .add_header("cookie", cookie)
        .json(&json!({ "code": code(&secret, 0) }))
        .await
        .json();
    assert_eq!(off["enabled"], false);
    login(&server, "rep@acme.test", PASSWORD)
        .await
        .assert_status_ok();
}

#[test_db::test(seed)]
async fn five_wrong_codes_end_the_sign_in(pool: MySqlPool) {
    let server = server(pool.clone());
    let cookie = session_cookie(&login(&server, "rep@acme.test", PASSWORD).await).unwrap();
    let secret = enable(&server, &cookie).await;
    let challenge: Value = login(&server, "rep@acme.test", PASSWORD).await.json();
    let token = challenge["token"].as_str().unwrap();
    for _ in 0..4 {
        server
            .post("/v1/auth/login/code")
            .json(&json!({ "token": token, "code": "000000" }))
            .await
            .assert_status_bad_request();
    }
    server
        .post("/v1/auth/login/code")
        .json(&json!({ "token": token, "code": "000000" }))
        .await
        .assert_status_unauthorized();
    server
        .post("/v1/auth/login/code")
        .json(&json!({ "token": token, "code": code(&secret, 1) }))
        .await
        .assert_status_unauthorized();
}

#[test_db::test(seed)]
async fn superusers_must_set_up_two_step_sign_in(pool: MySqlPool) {
    make_superuser(&pool, 102).await;
    set_password(&pool, 102, "a-new-password").await;
    let server = server(pool.clone());
    let started = login(&server, "admin@acme.test", "a-new-password").await;
    started.assert_status(StatusCode::ACCEPTED);
    assert!(session_cookie(&started).is_none());
    let challenge: Value = started.json();
    assert_eq!(challenge["step"], "setup_authenticator");
    let token = challenge["token"].as_str().unwrap().to_string();
    // A step other than the one needed is refused.
    server
        .post("/v1/auth/login/code")
        .json(&json!({ "token": token, "code": "123456" }))
        .await
        .assert_status(StatusCode::CONFLICT);

    let setup: Value = server
        .post("/v1/auth/login/authenticator")
        .json(&json!({ "token": token }))
        .await
        .json();
    let secret = setup["secret"].as_str().unwrap().to_string();
    assert_eq!(secret.len(), 32);
    assert!(
        setup["uri"]
            .as_str()
            .unwrap()
            .starts_with("otpauth://totp/Granite%20Manager:admin%40acme%2Etest?secret=")
    );
    assert!(
        setup["qr"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,")
    );
    // Asking again shows the same secret.
    let again: Value = server
        .post("/v1/auth/login/authenticator")
        .json(&json!({ "token": token }))
        .await
        .json();
    assert_eq!(again["secret"], secret.as_str());
    server
        .post("/v1/auth/login/authenticator/confirm")
        .json(&json!({ "token": token, "code": "000000" }))
        .await
        .assert_status_bad_request();
    let done = server
        .post("/v1/auth/login/authenticator/confirm")
        .json(&json!({ "token": token, "code": code(&secret, 0) }))
        .await;
    done.assert_status_ok();
    let cookie = session_cookie(&done).unwrap();
    let status: Value = server
        .get("/v1/me/mfa")
        .add_header("cookie", cookie.clone())
        .await
        .json();
    assert_eq!(status["enabled"], true);
    assert_eq!(status["required"], true);
    // A superuser cannot turn it off.
    server
        .post("/v1/me/mfa/disable")
        .add_header("cookie", cookie)
        .json(&json!({ "code": code(&secret, 1) }))
        .await
        .assert_status_forbidden();

    // From now on, a code.
    let challenge: Value = login(&server, "admin@acme.test", "a-new-password")
        .await
        .json();
    assert_eq!(challenge["step"], "code");
}

#[test_db::test(seed)]
async fn the_app_signs_in_through_the_steps(pool: MySqlPool) {
    make_superuser(&pool, 100).await;
    let server = server(pool.clone());
    let sign_in = |steps: bool| {
        json!({
            "email": "rep@acme.test",
            "password": PASSWORD,
            "deviceName": "Rita's Pixel",
            "platform": "android",
            "appVersion": "1.9.0",
            "steps": steps,
        })
    };
    // An app that cannot do the steps is asked to update.
    let old = server
        .post("/v1/auth/device-sessions")
        .json(&sign_in(false))
        .await;
    old.assert_status_forbidden();
    assert!(
        old.json::<Value>()["error"]
            .as_str()
            .unwrap()
            .starts_with("Update Granite Manager")
    );

    let challenge = server
        .post("/v1/auth/device-sessions")
        .json(&sign_in(true))
        .await;
    challenge.assert_status(StatusCode::ACCEPTED);
    let challenge: Value = challenge.json();
    assert_eq!(challenge["step"], "setup_authenticator");
    let token = challenge["token"].as_str().unwrap();
    let setup: Value = server
        .post("/v1/auth/login/authenticator")
        .json(&json!({ "token": token }))
        .await
        .json();
    let secret = setup["secret"].as_str().unwrap();
    let done = server
        .post("/v1/auth/login/authenticator/confirm")
        .json(&json!({ "token": token, "code": code(secret, 0) }))
        .await;
    done.assert_status(StatusCode::CREATED);
    assert!(session_cookie(&done).is_none());
    let session: Value = done.json();
    assert_eq!(session["user"]["id"], 100);
    let device_token = session["token"].as_str().unwrap().to_string();
    let (name, platform, version): (String, String, Option<String>) = sqlx::query_as(
        "SELECT device_name, CAST(platform AS CHAR), app_version FROM device_sessions
          WHERE CAST(id AS SIGNED) = ?",
    )
    .bind(session["sessionId"].as_i64().unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (name.as_str(), platform.as_str(), version.as_deref()),
        ("Rita's Pixel", "android", Some("1.9.0"))
    );

    // The app's device token reads and changes two-step sign-in too.
    let status: Value = server
        .get("/v1/me/mfa")
        .add_header("x-granite-device", device_token)
        .await
        .json();
    assert_eq!(status["enabled"], true);
    assert_eq!(status["required"], true);
}
