//! Replacing `SESSION_SECRET` (`api::session_secrets`): cookies signed with
//! the old secret keep signing people in, and come back signed with the new
//! one; nothing else opens.

use crate::common;

use api::app;
use api::auth::cookie::{SessionData, cookie_values, decode_session, encode_session};
use api::state::AppState;
use axum::http::StatusCode;
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use sqlx::MySqlPool;

const NEW: &str = "new-session-secret-new-session-secret";

/// Signs with `current`, also accepts `other`.
fn server(pool: MySqlPool, current: &str, other: &str, switched_at: Option<i64>) -> TestServer {
    TestServer::new(app(AppState::new(pool, current, ALLOWED_ORIGIN)
        .with_rotated_session_secret(current, other, switched_at)))
}

fn cookie_under(secret: &str, session_id: &str) -> String {
    let data = SessionData {
        session_id: Some(session_id.into()),
        active_company_id: None,
    };
    format!("__session={}", encode_session(&data, secret))
}

/// The session the response's `__session` cookie holds, under `secret`.
fn set_session(response: &axum_test::TestResponse, secret: &str) -> Option<SessionData> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next())
        .flat_map(|pair| cookie_values(pair, "__session").collect::<Vec<_>>())
        .find_map(|value| decode_session(&value, secret))
}

#[test_db::test(seed)]
async fn an_old_cookie_signs_in_and_comes_back_new(pool: MySqlPool) {
    let now = chrono::Utc::now().timestamp();
    // Months after the switch: long past the hour short-lived values get.
    let server = server(pool, NEW, SECRET, Some(now - 90 * 24 * 3600));
    let response = server
        .get("/v1/auth/session")
        .with_session(sessions::REP)
        .await;
    response.assert_status_ok();
    let renewed = set_session(&response, NEW).expect("a cookie under the new secret");
    assert_eq!(renewed.session_id.as_deref(), Some(sessions::REP));
    assert!(set_session(&response, SECRET).is_none());
    // The renewed cookie works, and so does the old one on any route.
    server
        .get("/v1/auth/session")
        .add_header("cookie", cookie_under(NEW, sessions::REP))
        .await
        .assert_status_ok();
    server
        .get("/v1/me")
        .with_session(sessions::MANAGER)
        .await
        .assert_status_ok();
}

#[test_db::test(seed)]
async fn a_super_admins_company_choice_survives_the_switch(pool: MySqlPool) {
    let server = server(pool, NEW, SECRET, Some(1));
    let data = SessionData {
        session_id: Some(sessions::SUPER_ADMIN.into()),
        active_company_id: Some(101),
    };
    let old = format!("theme=dark; __session={}", encode_session(&data, SECRET));
    let response = server
        .get("/v1/auth/session")
        .add_header("cookie", old)
        .await;
    response.assert_status_ok();
    assert_eq!(set_session(&response, NEW), Some(data));
}

#[test_db::test(seed)]
async fn before_the_switch_the_next_secrets_cookies_work(pool: MySqlPool) {
    // Step 1: still signing with the old secret, the new one accepted.
    let server = server(pool, SECRET, NEW, None);
    server
        .get("/v1/auth/session")
        .add_header("cookie", cookie_under(NEW, sessions::REP))
        .await
        .assert_status_ok();
    server
        .get("/v1/auth/session")
        .with_session(sessions::REP)
        .await
        .assert_status_ok();
}

#[test_db::test(seed)]
async fn a_cookie_under_neither_secret_signs_nobody_in(pool: MySqlPool) {
    let server = server(pool.clone(), NEW, SECRET, Some(1));
    server
        .get("/v1/auth/session")
        .add_header("cookie", cookie_under("some-other-secret", sessions::REP))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    // A signed-out session stays signed out under the old secret too.
    server
        .get("/v1/auth/session")
        .with_session(sessions::LOGGED_OUT)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    // Without the old secret configured, its cookies sign nobody in.
    TestServer::new(app(AppState::new(pool, NEW, ALLOWED_ORIGIN)))
        .get("/v1/auth/session")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}
