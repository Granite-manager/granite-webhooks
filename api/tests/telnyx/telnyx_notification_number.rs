//! The Notification number is one way, yet no customer's answer is lost:
//! texts that wait for an answer (a scheduling text, its follow-up, the
//! "YES or NO?" question) go out from the Scheduling Manager's line, and a
//! reply that still reaches the Notification number lands on that line.
//! Reminders keep going out from the Notification number.
//!
//! Acme (100) texts through Telnyx. Rita (100) is its Scheduling Manager
//! (position 13). 317-555-0161 (9711) is the Scheduling Manager's line,
//! 317-555-0162 (9712) the Notification number, 317-555-0163 (9713) Rita's
//! own line. Alice (1000) is 317-555-0100.

use crate::common;
use crate::phone_common;

use api::ai::Ai;
use api::app;
use api::state::AppState;
use api::telnyx::Telnyx;
use axum::Json;
use axum::Router;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use phone_common::{Signer, add_number, assign, enable_calling, signed_config, webhook_token};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const KEY: &str = "test-telnyx-key";
const SCHEDULING: &str = "+13175550161";
const NOTIFICATION: &str = "+13175550162";
const OWN: &str = "+13175550163";
const ALICE: &str = "+13175550100";

/// A stand-in for Telnyx (`POST /telnyx/messages`) and the AI (`/chat`).
#[derive(Clone, Default)]
struct Stub {
    /// The `from` of each text sent through Telnyx.
    from: Arc<Mutex<Vec<String>>>,
    sent: Arc<AtomicUsize>,
    /// What the AI answers.
    ai_reply: Arc<Mutex<String>>,
}

impl Stub {
    fn from(&self) -> Vec<String> {
        self.from.lock().unwrap().clone()
    }

    fn ai(&self, answer: &str, evidence: &str) {
        *self.ai_reply.lock().unwrap() =
            json!({ "answer": answer, "evidence": evidence }).to_string();
    }

    async fn answer(&self, request: Request) -> Response {
        let (parts, body) = request.into_parts();
        let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
        match parts.uri.path() {
            "/telnyx/messages" => {
                let body: Value = serde_json::from_slice(&bytes).unwrap();
                self.from
                    .lock()
                    .unwrap()
                    .push(body["from"].as_str().unwrap_or_default().to_string());
                let n = self.sent.fetch_add(1, Ordering::SeqCst) + 1;
                Json(
                    json!({ "data": { "id": format!("msg-{n}"), "record_type": "message",
                    "to": [{ "phone_number": body["to"], "status": "queued" }] } }),
                )
                .into_response()
            }
            "/chat" => {
                let content = self.ai_reply.lock().unwrap().clone();
                Json(json!({
                    "choices": [{ "message": { "content": content } }],
                    "usage": { "prompt_tokens": 100, "completion_tokens": 10 },
                }))
                .into_response()
            }
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    }
}

struct Setup {
    server: TestServer,
    stub: Stub,
    signer: Signer,
    /// Replies so far: each comes 5 seconds after the one before, later than
    /// the texts sent in the same second.
    replies: AtomicUsize,
}

const FIXTURE: &str = "
UPDATE telnyx_company_config SET messaging_profile_id = 'profile-100' WHERE company_id = 100;
INSERT INTO users_positions (user_id, position_id, company_id) VALUES (100, 13, 100);
INSERT INTO calendars (id, company_id, name, color, slug) VALUES (8830, 100, 'Installations', 'red', 'installations');
UPDATE company SET schedule_ai_ready = 1, schedule_text_days = 127,
       schedule_text_start = '00:00:00', schedule_text_end = '23:59:00' WHERE id = 100;
";

/// Which lines Acme has: the Scheduling Manager's, the Notification number,
/// and Rita's own; Rita may text from each.
struct Lines {
    scheduling: bool,
    own: bool,
}

async fn setup(pool: &MySqlPool, lines: Lines) -> Setup {
    enable_calling(pool, 100, "").await;
    sqlx::raw_sql(FIXTURE).execute(pool).await.unwrap();
    add_number(pool, 9712, 100, NOTIFICATION, "shared", "").await;
    sqlx::query("UPDATE telnyx_numbers SET notifications = 1 WHERE id = 9712")
        .execute(pool)
        .await
        .unwrap();
    assign(pool, 9712, 100, true, true).await;
    if lines.scheduling {
        add_number(pool, 9711, 100, SCHEDULING, "shared", "").await;
        sqlx::query("UPDATE telnyx_numbers SET position_id = 13 WHERE id = 9711")
            .execute(pool)
            .await
            .unwrap();
        assign(pool, 9711, 100, true, true).await;
    }
    if lines.own {
        add_number(pool, 9713, 100, OWN, "direct", "Rita").await;
        assign(pool, 9713, 100, true, true).await;
    }
    let stub = Stub::default();
    let handler = stub.clone();
    let router = Router::new().fallback(move |request: Request| {
        let handler = handler.clone();
        async move { handler.answer(request).await }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let signer = Signer::new();
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_telnyx(Telnyx::new(Some(KEY), &format!("http://{address}/telnyx")))
        .with_ai(Ai::new(Some("key"), &format!("http://{address}/chat")))
        .with_job_key("job-secret")
        .with_phone_config(signed_config(&signer.public_key));
    Setup {
        server: TestServer::new(app(state)),
        stub,
        signer,
        replies: AtomicUsize::new(0),
    }
}

impl Setup {
    /// The customer texts `to`, signed like Telnyx does.
    async fn reply(&self, id: &str, to: &str, text: &str) {
        let n = self.replies.fetch_add(1, Ordering::SeqCst) + 1;
        let at = chrono::Utc::now() + chrono::Duration::seconds(i64::try_from(n).unwrap() * 5);
        let event = json!({
            "data": { "event_type": "message.received", "id": format!("ev-{id}"),
              "occurred_at": at.to_rfc3339(), "record_type": "event",
              "payload": { "id": id, "direction": "inbound", "type": "SMS", "text": text,
                "from": { "phone_number": ALICE },
                "to": [{ "phone_number": to, "status": "webhook_delivered" }],
                "media": [], "messaging_profile_id": "profile-100" } },
            "meta": { "attempt": 1 }
        });
        let body = serde_json::to_vec(&event).unwrap();
        let [(sig_name, sig), (ts_name, ts)] = self.signer.headers(&body);
        self.server
            .post(&format!(
                "/v1/webhooks/telnyx/{}/messaging",
                webhook_token(100)
            ))
            .add_header(sig_name, sig)
            .add_header(ts_name, ts)
            .content_type("application/json")
            .bytes(body.into())
            .await
            .assert_status_ok();
    }

    async fn run_job(&self) -> Value {
        self.server
            .post("/v1/jobs/appointment-reminders")
            .add_header("Authorization", "job-secret")
            .await
            .json()
    }

    /// An auto-confirm text (with a follow-up after 2 hours) for the
    /// installations; returns its id.
    async fn question_template(&self) -> i64 {
        let created: Value = self
            .server
            .post("/v1/schedule/notifications/templates")
            .with_session(sessions::ADMIN)
            .json(&json!({
                "name": "Install date", "kind": "question", "channel": "sms",
                "body": "Hi {{customer.first_name}}, can we install {{appointment.date}}? Reply YES.",
                "calendar_ids": [8830],
                "followup": { "template_id": null, "after_hours": 2 },
            }))
            .await
            .json();
        created["id"].as_i64().unwrap()
    }

    async fn confirm(&self, event: i32, template: i64) {
        self.server
            .post(&format!("/v1/events/{event}/confirmation"))
            .with_session(sessions::REP)
            .json(&json!({ "template_id": template }))
            .await
            .assert_status_ok();
    }
}

/// Alice's installation `hours` from now, made long ago; its id.
async fn event(pool: &MySqlPool, hours: i32) -> i32 {
    let id = sqlx::query(
        "INSERT INTO events (title, start_date, end_date, created_user_id, calendar_id, location,
                             created_date, customer_id)
         VALUES ('1000', UTC_TIMESTAMP() + INTERVAL ? HOUR, UTC_TIMESTAMP() + INTERVAL ? HOUR, 100,
                 8830, '1 Main St', '2000-01-01', 1000)",
    )
    .bind(hours)
    .bind(hours + 2)
    .execute(pool)
    .await
    .unwrap()
    .last_insert_id();
    i32::try_from(id).unwrap()
}

/// Alice's inbound texts: (line, the 10 digits she texted, text).
async fn inbound(pool: &MySqlPool) -> Vec<(Option<i32>, Option<String>, String)> {
    sqlx::query_as(
        "SELECT number_id, recipient10, text FROM telnyx_sms
          WHERE direction = 'inbound' AND sender10 = '3175550100' ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// The lines Alice's outbound texts went from, oldest first.
async fn outbound_lines(pool: &MySqlPool) -> Vec<Option<i32>> {
    sqlx::query_scalar(
        "SELECT number_id FROM telnyx_sms
          WHERE direction = 'outbound' AND recipient10 = '3175550100' ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn status(pool: &MySqlPool, event: i32) -> String {
    sqlx::query_scalar("SELECT COALESCE(status, '') FROM events WHERE id = ?")
        .bind(event)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[test_db::test(seed)]
async fn auto_confirm_texts_go_from_the_scheduling_line_and_every_reply_is_read(pool: MySqlPool) {
    let setup = setup(
        &pool,
        Lines {
            scheduling: true,
            own: true,
        },
    )
    .await;
    let template = setup.question_template().await;
    let install = event(&pool, 72).await;

    // The scheduling text goes from the Scheduling Manager's line.
    setup.confirm(install, template).await;
    assert_eq!(setup.stub.from(), [SCHEDULING]);
    assert_eq!(status(&pool, install).await, "called");

    // So does its follow-up.
    sqlx::query(
        "UPDATE schedule_text_answers SET followup_due_at = UTC_TIMESTAMP() - INTERVAL 1 MINUTE",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(setup.run_job().await["followups_sent"], 1);
    assert_eq!(setup.stub.from(), [SCHEDULING, SCHEDULING]);

    // Alice answers the Notification number (an old text came from it): the
    // reply is kept, on the Scheduling Manager's line, in her thread there.
    setup.reply("in-1", NOTIFICATION, "ues").await;
    assert_eq!(
        inbound(&pool).await,
        [(Some(9711), Some("3175550161".into()), "ues".into())]
    );
    // A redelivery is stored once.
    setup.reply("in-1", NOTIFICATION, "ues").await;
    assert_eq!(inbound(&pool).await.len(), 1);

    // Unclear: the YES-or-NO question goes from the Scheduling Manager's line.
    setup.stub.ai("unclear", "ues");
    let run = setup.run_job().await;
    assert_eq!(run["answers_read"], 1);
    assert_eq!(setup.stub.from(), [SCHEDULING, SCHEDULING, SCHEDULING]);
    assert_eq!(status(&pool, install).await, "called");

    // Her YES reaches the Notification number too, and confirms the job.
    setup.reply("in-2", NOTIFICATION, "Yes").await;
    setup.stub.ai("agreed", "Yes");
    let run = setup.run_job().await;
    assert_eq!(run["marked_ready"], 1);
    assert_eq!(status(&pool, install).await, "confirmed");
    assert_eq!(
        inbound(&pool).await,
        [
            (Some(9711), Some("3175550161".into()), "ues".into()),
            (Some(9711), Some("3175550161".into()), "Yes".into()),
        ]
    );
    // Every text to Alice is on the Scheduling Manager's line, none on the
    // Notification number.
    assert_eq!(outbound_lines(&pool).await, [Some(9711); 3]);
}

#[test_db::test(seed)]
async fn reminders_still_go_from_the_notification_number(pool: MySqlPool) {
    let setup = setup(
        &pool,
        Lines {
            scheduling: true,
            own: false,
        },
    )
    .await;
    let template = sqlx::query(
        "INSERT INTO schedule_notification_templates (company_id, name, channel, body)
         VALUES (100, 'Reminder', 'sms', 'Hi {{customer.first_name}}, see you {{appointment.date}}.')",
    )
    .execute(&pool)
    .await
    .unwrap()
    .last_insert_id();
    let flow = sqlx::query(
        "INSERT INTO schedule_notification_flows (company_id, calendar_id, name, is_default)
         VALUES (100, 8830, 'Reminders', 1)",
    )
    .execute(&pool)
    .await
    .unwrap()
    .last_insert_id();
    sqlx::query(
        "INSERT INTO schedule_notification_rules
           (company_id, flow_id, template_id, offset_value, offset_unit, created_at)
         VALUES (100, ?, ?, 1, 'days', '2000-01-01')",
    )
    .bind(flow)
    .bind(template)
    .execute(&pool)
    .await
    .unwrap();
    let job = event(&pool, 23).await;
    sqlx::query("UPDATE events SET status = 'confirmed' WHERE id = ?")
        .bind(job)
        .execute(&pool)
        .await
        .unwrap();

    assert_eq!(setup.run_job().await["rule_sent"], 1);
    assert_eq!(setup.stub.from(), [NOTIFICATION]);
    assert_eq!(outbound_lines(&pool).await, [Some(9712)]);

    // A reply to the reminder is not lost: it lands on the Scheduling
    // Manager's line.
    setup.reply("in-1", NOTIFICATION, "Thanks, see you").await;
    assert_eq!(
        inbound(&pool).await,
        [(
            Some(9711),
            Some("3175550161".into()),
            "Thanks, see you".into()
        )]
    );
}

#[test_db::test(seed)]
async fn without_a_scheduling_line_the_managers_own_line_waits_for_answers(pool: MySqlPool) {
    let setup = setup(
        &pool,
        Lines {
            scheduling: false,
            own: true,
        },
    )
    .await;
    // Rita's default line is the Notification number: still not used.
    sqlx::query(
        "INSERT INTO phone_user_settings (user_id, company_id, default_caller_number_id)
         VALUES (100, 100, 9712)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let template = setup.question_template().await;
    let install = event(&pool, 72).await;
    setup.confirm(install, template).await;
    assert_eq!(setup.stub.from(), [OWN]);

    // A reply to the Notification number is kept there (no Scheduling
    // Manager's line), never dropped, and still read.
    setup.reply("in-1", NOTIFICATION, "Yes").await;
    assert_eq!(
        inbound(&pool).await,
        [(Some(9712), Some("3175550162".into()), "Yes".into())]
    );
    setup.stub.ai("agreed", "Yes");
    assert_eq!(setup.run_job().await["marked_ready"], 1);
    assert_eq!(status(&pool, install).await, "confirmed");
}

#[test_db::test(seed)]
async fn with_only_the_notification_number_answers_are_still_kept(pool: MySqlPool) {
    let setup = setup(
        &pool,
        Lines {
            scheduling: false,
            own: false,
        },
    )
    .await;
    let template = setup.question_template().await;
    let install = event(&pool, 72).await;
    // Nothing else to text from: the scheduling text still goes out.
    setup.confirm(install, template).await;
    assert_eq!(setup.stub.from(), [NOTIFICATION]);
    setup.reply("in-1", NOTIFICATION, "Yes").await;
    assert_eq!(
        inbound(&pool).await,
        [(Some(9712), Some("3175550162".into()), "Yes".into())]
    );
    setup.stub.ai("agreed", "Yes");
    assert_eq!(setup.run_job().await["marked_ready"], 1);
    assert_eq!(status(&pool, install).await, "confirmed");
}

#[test_db::test(seed)]
async fn stop_to_the_notification_number_still_opts_out(pool: MySqlPool) {
    let setup = setup(
        &pool,
        Lines {
            scheduling: true,
            own: false,
        },
    )
    .await;
    setup.reply("in-1", NOTIFICATION, "STOP").await;
    let source: Option<String> = sqlx::query_scalar(
        "SELECT source FROM sms_opt_outs WHERE company_id = 100 AND phone10 = '3175550100'",
    )
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert_eq!(source.as_deref(), Some("stop"));
    // An opted-out phone gets no scheduling text.
    let template = setup.question_template().await;
    let install = event(&pool, 72).await;
    setup
        .server
        .post(&format!("/v1/events/{install}/confirmation"))
        .with_session(sessions::REP)
        .json(&json!({ "template_id": template }))
        .await
        .assert_status_not_ok();
    assert!(setup.stub.from().is_empty());
}
