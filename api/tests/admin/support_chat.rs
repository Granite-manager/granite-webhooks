use crate::common;

use axum::http::StatusCode;
use axum_test::TestServer;
use common::{WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// User 103 as a support team member: a superuser with two-step sign-in on
/// since before their session began.
async fn make_agent(pool: &MySqlPool) {
    sqlx::query("UPDATE users SET is_superuser = 1 WHERE id = 103")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO user_mfa (user_id, secret, enabled_at) VALUES (103, 'a:secret', '2000-01-01')",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn write(server: &TestServer, session: &str, body: Value) -> Value {
    let res = server
        .post("/v1/support/messages")
        .with_session(session)
        .add_header(
            "user-agent",
            "Mozilla/5.0 (Macintosh) Chrome/131.0 Safari/537.36",
        )
        .json(&body)
        .await;
    res.assert_status(StatusCode::CREATED);
    res.json()
}

async fn thread(server: &TestServer, session: &str) -> Value {
    let res = server.get("/v1/support/thread").with_session(session).await;
    res.assert_status_ok();
    res.json()
}

#[test_db::test(seed)]
async fn clients_write_and_the_team_answers(pool: MySqlPool) {
    make_agent(&pool).await;
    let (server, _) = common::server_with_storage(pool.clone());

    // Nobody wrote yet: nothing to poll for.
    let empty = thread(&server, sessions::REP).await;
    assert_eq!(empty["messages"], json!([]));
    assert_eq!(empty["active"], false);
    assert_eq!(empty["status"], Value::Null);

    let first = write(
        &server,
        sessions::REP,
        json!({
            "body": "  The quote PDF shows the wrong sink\nfor the Miller kitchen  ",
            "page_path": "/employee/deals/7",
            "posthog_session_id": "0193abcd-ef01-7000-8000-123456789abc",
            "replay_seconds": 412,
            "screen": "1440x900"
        }),
    )
    .await;
    assert_eq!(
        first["body"],
        "The quote PDF shows the wrong sink\nfor the Miller kitchen"
    );
    assert_eq!(first["from_support"], false);
    // What the team sees stays out of the client's copy.
    assert!(first.get("replay_url").is_none());
    let second = write(&server, sessions::REP, json!({ "body": "It's urgent" })).await;
    assert_eq!(second["conversation_id"], first["conversation_id"]);
    let conversation = first["conversation_id"].as_i64().unwrap();

    let mine = thread(&server, sessions::REP).await;
    assert_eq!(mine["messages"].as_array().unwrap().len(), 2);
    assert_eq!(mine["active"], true);
    assert_eq!(mine["status"], "open");
    // Another user sees none of it.
    let theirs = thread(&server, sessions::OTHER_COMPANY_REP).await;
    assert_eq!(theirs["messages"], json!([]));

    // The team's inbox.
    let inbox: Value = server
        .get("/v1/support/agent/conversations")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .json();
    assert_eq!(inbox["unchanged"], false);
    let row = &inbox["conversations"][0];
    assert_eq!(row["id"], conversation);
    assert_eq!(row["subject"], "The quote PDF shows the wrong sink");
    assert_eq!(row["unread"], true);
    assert_eq!(row["status"], "open");
    assert_eq!(row["last_message"], "It's urgent");
    assert_eq!(inbox["counts"]["unread"], 1);
    assert_eq!(inbox["counts"]["unassigned"], 1);
    // Polling with the version: nothing changed, nothing sent.
    let version = inbox["version"].as_str().unwrap().to_string();
    let again: Value = server
        .get("/v1/support/agent/conversations")
        .add_query_param("version", &version)
        .with_session(sessions::SUPER_ADMIN)
        .await
        .json();
    assert_eq!(again, json!({ "unchanged": true, "version": version }));
    let searched: Value = server
        .get("/v1/support/agent/conversations")
        .add_query_param("q", "nothing like this")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .json();
    assert_eq!(searched["conversations"], json!([]));

    // A request: the messages with where the client was, and who they are.
    let detail: Value = server
        .get(&format!("/v1/support/agent/conversations/{conversation}"))
        .with_session(sessions::SUPER_ADMIN)
        .await
        .json();
    let message = &detail["messages"][0];
    assert_eq!(message["page_path"], "/employee/deals/7");
    assert_eq!(
        message["replay_url"],
        "https://us.posthog.com/project/141685/replay/0193abcd-ef01-7000-8000-123456789abc?t=412"
    );
    assert_eq!(message["screen"], "1440x900");
    assert!(message["user_agent"].as_str().unwrap().contains("Chrome"));
    assert_eq!(detail["client"]["user_id"], 100);
    assert_eq!(
        detail["client"]["company_id"],
        detail["conversation"]["company_id"]
    );
    assert_eq!(
        detail["posthog_person_url"],
        "https://us.posthog.com/project/141685/person/100"
    );
    assert!(detail["history"].is_array());

    // Type, priority, assignee.
    let res = server
        .patch(&format!("/v1/support/agent/conversations/{conversation}"))
        .with_session(sessions::SUPER_ADMIN)
        .json(&json!({ "assignee_id": 100 }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    let res = server
        .patch(&format!("/v1/support/agent/conversations/{conversation}"))
        .with_session(sessions::SUPER_ADMIN)
        .json(&json!({ "category": "nonsense" }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    let changed: Value = server
        .patch(&format!("/v1/support/agent/conversations/{conversation}"))
        .with_session(sessions::SUPER_ADMIN)
        .json(&json!({ "category": "bug", "priority": "high", "assignee_id": 103 }))
        .await
        .json();
    assert_eq!(changed["category"], "bug");
    assert_eq!(changed["priority"], "high");
    assert_eq!(changed["assignee_id"], 103);
    let cleared: Value = server
        .patch(&format!("/v1/support/agent/conversations/{conversation}"))
        .with_session(sessions::SUPER_ADMIN)
        .json(&json!({ "category": null }))
        .await
        .json();
    assert_eq!(cleared["category"], Value::Null);
    assert_eq!(cleared["priority"], "high");

    // A note the client never sees, then an answer they do.
    server
        .post(&format!(
            "/v1/support/agent/conversations/{conversation}/messages"
        ))
        .with_session(sessions::SUPER_ADMIN)
        .json(&json!({ "body": "Cached revision, checking", "internal": true }))
        .await
        .assert_status(StatusCode::CREATED);
    let answer = server
        .post(&format!(
            "/v1/support/agent/conversations/{conversation}/messages"
        ))
        .with_session(sessions::SUPER_ADMIN)
        .json(&json!({ "body": "Fixed, print it again." }))
        .await;
    answer.assert_status(StatusCode::CREATED);
    let answer: Value = answer.json();

    let mine = thread(&server, sessions::REP).await;
    let bodies: Vec<&str> = mine["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["body"].as_str().unwrap())
        .collect();
    assert_eq!(
        bodies,
        [
            "The quote PDF shows the wrong sink\nfor the Miller kitchen",
            "It's urgent",
            "Fixed, print it again."
        ]
    );
    assert_eq!(mine["status"], "waiting");
    assert_eq!(mine["unread"], 1);
    // A poll after the newest message the client has: nothing more.
    let polled: Value = server
        .get("/v1/support/thread")
        .add_query_param("after", answer["id"].as_i64().unwrap())
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(polled["messages"], json!([]));
    // The Chat button's badge, then read.
    let badges: Value = server
        .get("/v1/me/badges?include=support_unread")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(badges["support_unread"]["count"], 1);
    server
        .post("/v1/support/thread/read")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(thread(&server, sessions::REP).await["unread"], 0);

    // Resolved: the next message starts a new request.
    server
        .patch(&format!("/v1/support/agent/conversations/{conversation}"))
        .with_session(sessions::SUPER_ADMIN)
        .json(&json!({ "status": "resolved" }))
        .await
        .assert_status_ok();
    let next = write(&server, sessions::REP, json!({ "body": "One more thing" })).await;
    assert_ne!(next["conversation_id"], conversation);
    let detail: Value = server
        .get(&format!(
            "/v1/support/agent/conversations/{}",
            next["conversation_id"]
        ))
        .with_session(sessions::SUPER_ADMIN)
        .await
        .json();
    assert_eq!(detail["history"][0]["id"], conversation);
    server
        .post(&format!(
            "/v1/support/agent/conversations/{}/read",
            next["conversation_id"]
        ))
        .with_session(sessions::SUPER_ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
}

#[test_db::test(seed)]
async fn only_superusers_signed_in_with_two_step_sign_in_use_the_app(pool: MySqlPool) {
    let server = common::server(pool.clone());
    let bootstrap = || server.get("/v1/support/agent/bootstrap");

    bootstrap().await.assert_status(StatusCode::UNAUTHORIZED);
    // An admin, not a superuser.
    bootstrap()
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    sqlx::query("UPDATE users SET is_superuser = 1 WHERE id = 103")
        .execute(&pool)
        .await
        .unwrap();
    // A superuser without two-step sign-in.
    let res = bootstrap().with_session(sessions::SUPER_ADMIN).await;
    res.assert_status(StatusCode::FORBIDDEN);
    assert!(res.text().contains("authenticator code"));
    // Turned on after this session began: the session never asked for a code.
    sqlx::query(
        "INSERT INTO user_mfa (user_id, secret, enabled_at) VALUES (103, 'a:secret', '2099-01-01')",
    )
    .execute(&pool)
    .await
    .unwrap();
    bootstrap()
        .with_session(sessions::SUPER_ADMIN)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    sqlx::query("UPDATE user_mfa SET enabled_at = '2000-01-01' WHERE user_id = 103")
        .execute(&pool)
        .await
        .unwrap();
    let res = bootstrap().with_session(sessions::SUPER_ADMIN).await;
    res.assert_status_ok();
    let body: Value = res.json();
    assert_eq!(body["me"]["id"], 103);
    assert!(
        body["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == 103)
    );
    assert_eq!(body["categories"][0]["value"], "how_to");
    // A token instead of the browser session: never the team's.
    bootstrap()
        .with_session(sessions::SUPER_ADMIN)
        .add_header("authorization", "Bearer gm_agent_whatever")
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .get("/v1/support/agent/conversations/999999")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn messages_are_checked(pool: MySqlPool) {
    let server = common::server(pool);
    for body in [
        json!({ "body": "   " }),
        json!({ "body": "x".repeat(5001) }),
        json!({ "body": "hi", "images": ["https://evil.test/a.png"] }),
    ] {
        server
            .post("/v1/support/messages")
            .with_session(sessions::REP)
            .json(&body)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    // Unsafe context is dropped, not stored.
    let res = server
        .post("/v1/support/messages")
        .with_session(sessions::REP)
        .json(&json!({
            "body": "hi",
            "page_path": "javascript:alert(1)",
            "posthog_session_id": "../../x",
            "screen": "<b>"
        }))
        .await;
    res.assert_status(StatusCode::CREATED);
}

/// A server whose AI answers with `subjects`, one per call, and records the
/// questions.
async fn server_with_ai(
    pool: MySqlPool,
    subjects: &[&str],
) -> (TestServer, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
    use axum::routing::post;
    use std::sync::{Arc, Mutex};
    let asked: Arc<Mutex<Vec<Value>>> = Arc::default();
    let answers: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(
        subjects.iter().rev().map(|s| (*s).to_string()).collect(),
    ));
    let router = axum::Router::new().route(
        "/chat",
        post({
            let asked = asked.clone();
            move |axum::Json(body): axum::Json<Value>| async move {
                asked.lock().unwrap().push(body);
                let content = answers.lock().unwrap().pop().unwrap_or_default();
                axum::Json(json!({
                    "choices": [{ "message": { "content": content } }],
                    "usage": { "prompt_tokens": 50, "completion_tokens": 5 },
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = api::state::AppState::new(pool, common::SECRET, common::ALLOWED_ORIGIN).with_ai(
        api::ai::Ai::new(Some("key"), &format!("http://{address}/chat")),
    );
    (TestServer::new(api::app(state)), asked)
}

#[test_db::test(seed)]
async fn each_conversation_is_its_own_request_named_by_the_ai(pool: MySqlPool) {
    make_agent(&pool).await;
    let (server, asked) = server_with_ai(
        pool.clone(),
        &[
            "\"Quote PDF shows the old sink.\"",
            "Add a second calendar",
            "Import customers from a CSV",
        ],
    )
    .await;

    // Nothing yet.
    let none: Value = server
        .get("/v1/support/conversations")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        none,
        json!({ "conversations": [], "active": false, "unread": 0 })
    );

    let first = write(
        &server,
        sessions::REP,
        json!({ "body": "When I print the Miller quote it shows the old sink", "new_conversation": true }),
    )
    .await;
    let quote = first["conversation_id"].as_i64().unwrap();
    // The AI saw the message and named it.
    assert_eq!(asked.lock().unwrap().len(), 1);
    assert!(
        asked.lock().unwrap()[0]
            .to_string()
            .contains("Miller quote")
    );

    // "hi" says nothing: the second message names the conversation.
    let hi = write(
        &server,
        sessions::REP,
        json!({ "body": "hi", "new_conversation": true }),
    )
    .await;
    let calendar = hi["conversation_id"].as_i64().unwrap();
    assert_ne!(calendar, quote);
    write(
        &server,
        sessions::REP,
        json!({ "body": "how do I add a second calendar?", "conversation_id": calendar }),
    )
    .await;
    assert_eq!(asked.lock().unwrap().len(), 3);

    let list: Value = server
        .get("/v1/support/conversations")
        .with_session(sessions::REP)
        .await
        .json();
    let subjects: Vec<&str> = list["conversations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["subject"].as_str().unwrap())
        .collect();
    assert_eq!(
        subjects,
        [
            "Import customers from a CSV",
            "Quote PDF shows the old sink"
        ]
    );
    assert_eq!(list["active"], true);
    // The team sees two separate requests.
    let inbox: Value = server
        .get("/v1/support/agent/conversations")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .json();
    assert_eq!(inbox["conversations"].as_array().unwrap().len(), 2);

    // The team renames one, notes and answers in the other.
    let renamed: Value = server
        .patch(&format!("/v1/support/agent/conversations/{calendar}"))
        .with_session(sessions::SUPER_ADMIN)
        .json(&json!({ "subject": "Second calendar for installers" }))
        .await
        .json();
    assert_eq!(renamed["subject"], "Second calendar for installers");
    server
        .patch(&format!("/v1/support/agent/conversations/{calendar}"))
        .with_session(sessions::SUPER_ADMIN)
        .json(&json!({ "subject": "  " }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    for (body, internal) in [("Secret note", true), ("Print it again now.", false)] {
        server
            .post(&format!("/v1/support/agent/conversations/{quote}/messages"))
            .with_session(sessions::SUPER_ADMIN)
            .json(&json!({ "body": body, "internal": internal }))
            .await
            .assert_status(StatusCode::CREATED);
    }

    let list: Value = server
        .get("/v1/support/conversations")
        .with_session(sessions::REP)
        .await
        .json();
    let row = list["conversations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == quote)
        .unwrap()
        .clone();
    assert_eq!(row["unread"], true);
    assert_eq!(row["status"], "waiting");
    assert_eq!(row["last_message"], "Print it again now.");
    assert_eq!(list["unread"], 1);
    assert!(!list.to_string().contains("Secret note"));

    // One conversation: its own messages only, never the note.
    let detail: Value = server
        .get(&format!("/v1/support/conversations/{quote}"))
        .with_session(sessions::REP)
        .await
        .json();
    let bodies: Vec<&str> = detail["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["body"].as_str().unwrap())
        .collect();
    assert_eq!(
        bodies,
        [
            "When I print the Miller quote it shows the old sink",
            "Print it again now."
        ]
    );
    let newest = detail["messages"][1]["id"].as_i64().unwrap();
    let polled: Value = server
        .get(&format!("/v1/support/conversations/{quote}"))
        .add_query_param("after", newest)
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(polled["messages"], json!([]));
    server
        .post(&format!("/v1/support/conversations/{quote}/read"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let after: Value = server
        .get("/v1/support/conversations")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(after["unread"], 0);

    // Resolved, then written in again: the same request opens again.
    server
        .patch(&format!("/v1/support/agent/conversations/{quote}"))
        .with_session(sessions::SUPER_ADMIN)
        .json(&json!({ "status": "resolved" }))
        .await
        .assert_status_ok();
    let again = write(
        &server,
        sessions::REP,
        json!({ "body": "It happened again", "conversation_id": quote }),
    )
    .await;
    assert_eq!(again["conversation_id"], quote);
    let reopened: Value = server
        .get(&format!("/v1/support/conversations/{quote}"))
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(reopened["conversation"]["status"], "open");

    // Someone else's conversation does not exist for them.
    server
        .get(&format!("/v1/support/conversations/{quote}"))
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .post("/v1/support/messages")
        .with_session(sessions::OTHER_COMPANY_REP)
        .json(&json!({ "body": "sneaky", "conversation_id": quote }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .post(&format!("/v1/support/conversations/{quote}/read"))
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}
