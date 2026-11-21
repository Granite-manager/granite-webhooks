//! Event details > Scheduling call (`notifications::calls`,
//! `telnyx_webhooks/schedule_call.rs`): one automatic call reads the
//! auto-confirm text and takes a yes, a no or "repeat" by fixed rules; a
//! call that brings no yes or no is followed by the scheduling text.
//!
//! Acme (100) calls and texts through Telnyx. Rita (100) is its Scheduling
//! Manager (position 13); 317-555-0161 (9721) is the Scheduling Manager's
//! line. Alice (1000) is 317-555-0100.

use crate::common;
use crate::phone_common;

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
use phone_common::{
    Signer, add_number, assign, enable_calling, post_texml, signed_config, webhook_base,
};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};

const SCHEDULING: &str = "+13175550161";
const ALICE: &str = "+13175550100";

/// A stand-in for Telnyx: texts (`POST /telnyx/messages`) and TeXML calls.
#[derive(Clone, Default)]
struct Stub {
    /// Each text sent: (from, text).
    texts: Arc<Mutex<Vec<(String, String)>>>,
    /// Each TeXML request: (path, body).
    calls: Arc<Mutex<Vec<(String, Value)>>>,
}

impl Stub {
    fn texts(&self) -> Vec<(String, String)> {
        self.texts.lock().unwrap().clone()
    }

    fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }

    async fn answer(&self, request: Request) -> Response {
        let (parts, body) = request.into_parts();
        let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let path = parts.uri.path().to_string();
        if path == "/telnyx/messages" {
            let text = |key: &str| body[key].as_str().unwrap_or_default().to_string();
            let n = {
                let mut texts = self.texts.lock().unwrap();
                texts.push((text("from"), text("text")));
                texts.len()
            };
            return Json(
                json!({ "data": { "id": format!("msg-{n}"), "record_type": "message",
                    "to": [{ "phone_number": body["to"], "status": "queued" }] } }),
            )
            .into_response();
        }
        if path.starts_with("/telnyx/texml/calls/") {
            let n = {
                let mut calls = self.calls.lock().unwrap();
                calls.push((path, body));
                calls.len()
            };
            return Json(json!({ "data": { "call_sid": format!("v3:call-{n}") } })).into_response();
        }
        StatusCode::NOT_FOUND.into_response()
    }
}

const FIXTURE: &str = "
UPDATE telnyx_company_config SET messaging_profile_id = 'profile-100' WHERE company_id = 100;
INSERT INTO users_positions (user_id, position_id, company_id) VALUES (100, 13, 100);
INSERT INTO calendars (id, company_id, name, color, slug) VALUES (8840, 100, 'Installations', 'red', 'installations');
UPDATE company SET schedule_ai_ready = 1, schedule_text_days = 127,
       schedule_text_start = '00:00:00', schedule_text_end = '23:59:00' WHERE id = 100;
";

const TEXT: &str = "Hi {{customer.first_name}}, this is Acme. Can we install {{appointment.date}}? \
                    Please reply YES to confirm, or NO and we'll find another time.";

struct Setup {
    server: TestServer,
    stub: Stub,
    /// The auto-confirm text.
    template: i64,
}

async fn setup(pool: &MySqlPool) -> Setup {
    enable_calling(pool, 100, "").await;
    sqlx::raw_sql(FIXTURE).execute(pool).await.unwrap();
    add_number(pool, 9721, 100, SCHEDULING, "shared", "").await;
    sqlx::query("UPDATE telnyx_numbers SET position_id = 13 WHERE id = 9721")
        .execute(pool)
        .await
        .unwrap();
    assign(pool, 9721, 100, true, true).await;
    let stub = Stub::default();
    let handler = stub.clone();
    let router = Router::new().fallback(move |request: Request| {
        let handler = handler.clone();
        async move { handler.answer(request).await }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_telnyx(Telnyx::new(
            Some("test-telnyx-key"),
            &format!("http://{address}/telnyx"),
        ))
        .with_job_key("job-secret")
        .with_phone_config(signed_config(&Signer::new().public_key));
    let server = TestServer::new(app(state));
    let created: Value = server
        .post("/v1/schedule/notifications/templates")
        .with_session(sessions::ADMIN)
        .json(&json!({
            "name": "Install date", "kind": "question", "channel": "sms", "body": TEXT,
            "calendar_ids": [8840],
            "followup": { "template_id": null, "after_hours": 2 },
        }))
        .await
        .json();
    Setup {
        server,
        stub,
        template: created["id"].as_i64().unwrap(),
    }
}

impl Setup {
    /// Places the Scheduling call for `event`; the call's id.
    async fn call(&self, pool: &MySqlPool, event: i32) -> i64 {
        self.server
            .post(&format!("/v1/events/{event}/confirmation-call"))
            .with_session(sessions::REP)
            .json(&json!({ "template_id": self.template }))
            .await
            .assert_status_ok();
        sqlx::query_scalar(
            "SELECT CAST(id AS SIGNED) FROM schedule_calls WHERE event_id = ? ORDER BY id DESC LIMIT 1",
        )
        .bind(event)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// A Telnyx callback of the call; the TeXML (or nothing) answered.
    async fn hook(&self, route: &str, call: i64, form: &[(&str, &str)]) -> String {
        let mut fields = vec![("CallSid", "v3:call-1")];
        fields.extend_from_slice(form);
        let res = post_texml(&self.server, 100, &format!("{route}?call={call}"), &fields).await;
        res.assert_status_ok();
        res.text()
    }

    /// The customer picks up.
    async fn pick_up(&self, call: i64) -> String {
        self.hook("schedule-call", call, &[("CallStatus", "in-progress")])
            .await
    }

    async fn says(&self, call: i64, words: &str) -> String {
        self.hook("schedule-call-answer", call, &[("SpeechResult", words)])
            .await
    }
}

/// Alice's installation `hours` from now; its id.
async fn event(pool: &MySqlPool, hours: i32) -> i32 {
    let id = sqlx::query(
        "INSERT INTO events (title, start_date, end_date, created_user_id, calendar_id, location,
                             created_date, customer_id)
         VALUES ('Alice kitchen', UTC_TIMESTAMP() + INTERVAL ? HOUR,
                 UTC_TIMESTAMP() + INTERVAL ? HOUR, 100, 8840, '1 Main St', '2000-01-01', 1000)",
    )
    .bind(hours)
    .bind(hours + 2)
    .execute(pool)
    .await
    .unwrap()
    .last_insert_id();
    i32::try_from(id).unwrap()
}

async fn status(pool: &MySqlPool, event: i32) -> String {
    sqlx::query_scalar("SELECT COALESCE(status, '') FROM events WHERE id = ?")
        .bind(event)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn history(pool: &MySqlPool, event: i32) -> Vec<String> {
    sqlx::query_scalar("SELECT message FROM event_history WHERE event_id = ? ORDER BY id")
        .bind(event)
        .fetch_all(pool)
        .await
        .unwrap()
}

/// What the Scheduling Manager (Rita) was told.
async fn notices(pool: &MySqlPool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT message FROM notifications
          WHERE user_id = 100 AND notification_type = 'schedule_ai_ready' ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn call_state(pool: &MySqlPool, call: i64) -> (String, i32, i32) {
    sqlx::query_as("SELECT CAST(status AS CHAR), tries, repeats FROM schedule_calls WHERE id = ?")
        .bind(call)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Texts waiting for Alice's answer about `event`.
async fn waiting(pool: &MySqlPool, event: i32) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM schedule_text_answers WHERE event_id = ? AND status = 'waiting'",
    )
    .bind(event)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[test_db::test(seed)]
async fn a_yes_on_the_call_confirms_the_job(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let install = event(&pool, 72).await;
    let base = webhook_base(100);

    let texts: Value = setup
        .server
        .get(&format!("/v1/events/{install}/scheduling-texts"))
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(texts["can_call"], true);

    let call = setup.call(&pool, install).await;
    // One call, from the Scheduling Manager's line, listening for a machine.
    let placed = setup.stub.calls();
    assert_eq!(placed.len(), 1);
    assert_eq!(placed[0].0, "/telnyx/texml/calls/app-100");
    let body = &placed[0].1;
    assert_eq!(body["To"], ALICE);
    assert_eq!(body["From"], SCHEDULING);
    assert_eq!(body["Url"], format!("{base}/schedule-call?call={call}"));
    assert_eq!(
        body["StatusCallback"],
        format!("{base}/schedule-call-status?call={call}")
    );
    assert_eq!(body["MachineDetection"], "Enable");
    assert_eq!(body["AsyncAmd"], true);
    assert_eq!(
        body["AsyncAmdStatusCallback"],
        format!("{base}/schedule-call-machine?call={call}")
    );
    assert_eq!(status(&pool, install).await, "called");
    assert_eq!(
        history(&pool, install).await,
        [
            "Confirmation call placed: Install date",
            "Status changed to Awaiting confirmation: the customer was called",
        ]
    );
    // Only one call at a time.
    let again = setup
        .server
        .post(&format!("/v1/events/{install}/confirmation-call"))
        .with_session(sessions::REP)
        .json(&json!({ "template_id": setup.template }))
        .await;
    again.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(setup.stub.calls().len(), 1);

    // Alice picks up: the text without its "reply YES" sentence, then the
    // question, in a <Gather> that takes a word or a key.
    let asked = setup.pick_up(call).await;
    assert!(
        asked.contains(&format!(
            "<Gather input=\"dtmf speech\" numDigits=\"1\" timeout=\"6\" speechTimeout=\"2\" \
             language=\"en-US\" hints=\"yes,no,repeat\" \
             action=\"{base}/schedule-call-answer?call={call}\">"
        )),
        "{asked}"
    );
    assert!(asked.contains("Hi Alice, this is Acme. Can we install "), "{asked}");
    assert!(!asked.to_lowercase().contains("reply"), "{asked}");
    assert!(
        asked.contains(
            "Say yes or press 1 to confirm. Say no or press 2. \
             If you&apos;d like to repeat this information, say repeat.</Say></Gather>"
        ),
        "{asked}"
    );
    assert!(
        asked.contains(&format!(
            "<Redirect>{base}/schedule-call-answer?call={call}</Redirect>"
        )),
        "{asked}"
    );

    // Something else: asked again.
    let unclear = setup.says(call, "Who is this?").await;
    assert!(unclear.contains("Sorry, I didn&apos;t understand your answer."), "{unclear}");
    assert_eq!(call_state(&pool, call).await, ("asking".into(), 2, 0));
    // "Repeat": the message again; it uses up no try.
    let repeated = setup.says(call, "Repeat").await;
    assert!(repeated.contains("Can we install "), "{repeated}");
    assert_eq!(call_state(&pool, call).await, ("asking".into(), 2, 1));

    // Yes: confirmed, and the Scheduling Manager is told. Nothing is texted.
    let thanks = setup.says(call, "Yes.").await;
    assert!(
        thanks.contains("Thank you, your appointment is confirmed. Goodbye.</Say><Hangup/>"),
        "{thanks}"
    );
    assert_eq!(status(&pool, install).await, "confirmed");
    assert_eq!(call_state(&pool, call).await.0, "yes");
    assert_eq!(
        notices(&pool).await,
        ["Alice kitchen is Confirmed: the customer said yes on the call"]
    );
    assert_eq!(
        history(&pool, install).await.last().unwrap(),
        "Status changed to Confirmed: the customer said yes on the call"
    );
    // The call ends; an answer that comes again changes nothing.
    setup
        .hook("schedule-call-status", call, &[("CallStatus", "completed")])
        .await;
    assert!(setup.says(call, "no").await.contains("<Hangup/>"));
    assert_eq!(status(&pool, install).await, "confirmed");
    assert_eq!(notices(&pool).await.len(), 1);
    assert!(setup.stub.texts().is_empty());
}

#[test_db::test(seed)]
async fn a_no_tells_the_scheduling_manager(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let install = event(&pool, 72).await;
    let call = setup.call(&pool, install).await;
    setup.pick_up(call).await;
    // The 2 key.
    let sorry = setup
        .hook("schedule-call-answer", call, &[("Digits", "2")])
        .await;
    assert!(
        sorry.contains(
            "Okay. I&apos;ll notify our scheduling manager, and they will contact you to find \
             another time. Goodbye.</Say><Hangup/>"
        ),
        "{sorry}"
    );
    assert_eq!(call_state(&pool, call).await.0, "no");
    assert_eq!(status(&pool, install).await, "called");
    assert_eq!(
        notices(&pool).await,
        ["Alice kitchen the customer said NO on the confirmation call"]
    );
    assert_eq!(
        history(&pool, install).await.last().unwrap(),
        "The customer said no on the confirmation call"
    );
    setup
        .hook("schedule-call-status", call, &[("CallStatus", "completed")])
        .await;
    assert!(setup.stub.texts().is_empty());
    assert_eq!(waiting(&pool, install).await, 0);
}

#[test_db::test(seed)]
async fn nobody_picking_up_gets_the_scheduling_text(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let install = event(&pool, 72).await;
    let call = setup.call(&pool, install).await;
    setup
        .hook("schedule-call-status", call, &[("CallStatus", "no-answer")])
        .await;
    assert_eq!(call_state(&pool, call).await.0, "unanswered");
    // The same text, with its "reply YES" sentence, from the same line; it
    // waits for her reply like any auto-confirm text.
    let texts = setup.stub.texts();
    assert_eq!(texts.len(), 1);
    assert_eq!(texts[0].0, SCHEDULING);
    assert!(texts[0].1.starts_with("Hi Alice, this is Acme. Can we install "));
    assert!(texts[0].1.ends_with("Please reply YES to confirm, or NO and we'll find another time."));
    assert_eq!(waiting(&pool, install).await, 1);
    assert_eq!(status(&pool, install).await, "called");
    let lines = history(&pool, install).await;
    assert_eq!(
        lines[lines.len() - 2..],
        [
            "Confirmation call: nobody picked up",
            "Scheduling text sent: the call brought no answer",
        ]
    );
    // The callback again, or a late pick-up, changes nothing.
    setup
        .hook("schedule-call-status", call, &[("CallStatus", "no-answer")])
        .await;
    assert!(setup.pick_up(call).await.contains("<Hangup/>"));
    assert_eq!(setup.stub.texts().len(), 1);
    assert!(notices(&pool).await.is_empty());

    // The call is over: the job can be called again.
    let second = setup.call(&pool, install).await;
    assert_ne!(second, call);
    // Busy.
    setup
        .hook("schedule-call-status", second, &[("CallStatus", "busy")])
        .await;
    assert_eq!(setup.stub.texts().len(), 2);
    assert!(
        history(&pool, install)
            .await
            .contains(&"Confirmation call: the line was busy".to_string())
    );
}

#[test_db::test(seed)]
async fn voicemail_and_unclear_answers_end_with_the_text(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let install = event(&pool, 72).await;

    // Voicemail picks up while the voice already speaks: hung up, texted.
    let call = setup.call(&pool, install).await;
    setup.pick_up(call).await;
    setup
        .hook("schedule-call-machine", call, &[("AnsweredBy", "human")])
        .await;
    assert_eq!(call_state(&pool, call).await.0, "asking");
    setup
        .hook("schedule-call-machine", call, &[("AnsweredBy", "machine_start")])
        .await;
    assert_eq!(call_state(&pool, call).await.0, "unanswered");
    let redirect = setup.stub.calls();
    assert_eq!(redirect.last().unwrap().0, "/telnyx/texml/calls/v3:call-1/update");
    assert!(setup.pick_up(call).await.contains("<Hangup/>"));
    assert_eq!(setup.stub.texts().len(), 1);
    assert!(
        history(&pool, install)
            .await
            .contains(&"Confirmation call: voicemail picked up".to_string())
    );

    // Three answers that are no yes or no (silence too): a text instead.
    let other = event(&pool, 96).await;
    let call = setup.call(&pool, other).await;
    setup.pick_up(call).await;
    assert!(setup.says(call, "maybe").await.contains("<Gather"));
    assert!(setup.hook("schedule-call-answer", call, &[]).await.contains("<Gather"));
    let bye = setup.says(call, "yes but can you come later").await;
    assert!(
        bye.contains(
            "Sorry, I still didn&apos;t understand. We&apos;ll send you a text message instead. \
             Goodbye.</Say><Hangup/>"
        ),
        "{bye}"
    );
    assert_eq!(call_state(&pool, call).await, ("unclear".into(), 3, 0));
    assert_eq!(setup.stub.texts().len(), 2);
    assert_eq!(waiting(&pool, other).await, 1);
    assert_eq!(status(&pool, other).await, "called");
    assert!(
        history(&pool, other)
            .await
            .contains(&"Confirmation call: no clear answer".to_string())
    );

    // Alice hangs up before answering: the text.
    let third = event(&pool, 120).await;
    let call = setup.call(&pool, third).await;
    setup.pick_up(call).await;
    setup
        .hook("schedule-call-status", call, &[("CallStatus", "completed")])
        .await;
    assert_eq!(setup.stub.texts().len(), 3);
    assert!(
        history(&pool, third)
            .await
            .contains(&"Confirmation call: the customer hung up before answering".to_string())
    );
    assert!(notices(&pool).await.is_empty());
}

#[test_db::test(seed)]
async fn calls_need_calling_and_an_auto_confirm_text(pool: MySqlPool) {
    let setup = setup(&pool).await;
    let install = event(&pool, 72).await;
    let post = |body: Value| {
        setup
            .server
            .post(&format!("/v1/events/{install}/confirmation-call"))
            .with_session(sessions::REP)
            .json(&body)
    };
    // Not an auto-confirm text.
    let plain: Value = setup
        .server
        .post("/v1/schedule/notifications/templates")
        .with_session(sessions::ADMIN)
        .json(&json!({
            "name": "See you", "kind": "confirmation", "channel": "sms",
            "body": "See you {{appointment.date}}.", "calendar_ids": [8840],
        }))
        .await
        .json();
    post(json!({ "template_id": plain["id"] }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    // An edited text with nothing to say.
    post(json!({ "template_id": setup.template, "text": "Reply YES or NO." }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    // Another company's event.
    setup
        .server
        .post(&format!("/v1/events/{install}/confirmation-call"))
        .with_session(sessions::OTHER_COMPANY_REP)
        .json(&json!({ "template_id": setup.template }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    // Calling switched off: no call, and the event details hide the button.
    sqlx::query("UPDATE telnyx_company_config SET enabled = 0 WHERE company_id = 100")
        .execute(&pool)
        .await
        .unwrap();
    let refused = post(json!({ "template_id": setup.template })).await;
    refused.assert_status(StatusCode::BAD_REQUEST);
    assert!(refused.text().contains("Granite Manager phone line"));
    let texts: Value = setup
        .server
        .get(&format!("/v1/events/{install}/scheduling-texts"))
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(texts["can_call"], false);
    assert!(setup.stub.calls().is_empty());
    assert!(history(&pool, install).await.is_empty());
    // A callback for a call that does not exist hangs up.
    assert!(setup.pick_up(999_999).await.contains("<Hangup/>"));
}
