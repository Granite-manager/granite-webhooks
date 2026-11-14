use crate::common;

use api::raptor::sync::{Config, run, sold, unsold};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// Seed: in company 100 Rita (100) owns the open deal 8002 "Erin countertops"
// of Erin Builder (1004). Winning it with "Project on a sale" on starts a
// project; its quote below has one kitchen counter with a sink.

const KEY: [u8; 32] = [7; 32];

#[derive(Clone, Default)]
struct Fake {
    calls: Arc<Mutex<Vec<(String, String, Value)>>>,
    next_job: Arc<AtomicU32>,
    note_fails: Arc<AtomicBool>,
    delete_fails: Arc<AtomicBool>,
    /// The notes Raptor holds, by job.
    notes: Arc<Mutex<Vec<(String, Value)>>>,
}

impl Fake {
    fn log(&self, method: &str, path: String, body: Value) {
        self.calls
            .lock()
            .unwrap()
            .push((method.to_string(), path, body));
    }

    fn calls(&self, method: &str, prefix: &str) -> Vec<(String, Value)> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, p, _)| m == method && p.starts_with(prefix))
            .map(|(_, p, b)| (p.clone(), b.clone()))
            .collect()
    }
}

/// Raptor's API as its website uses it, with one numbered job already there.
async fn fake_raptor() -> (String, Fake) {
    let fake = Fake::default();
    let app = Router::new()
        .route(
            "/auth/login",
            post(|Json(body): Json<Value>| async move {
                if body["password"] == "secret" {
                    Ok(Json(json!({ "id": "user-1", "token": "t-1" })))
                } else {
                    Err(StatusCode::UNAUTHORIZED)
                }
            }),
        )
        .route(
            "/user/bootstrap",
            get(|| async { Json(json!({ "companies": [{ "id": "co-1" }] })) }),
        )
        .route(
            "/entity/co-1/bootstrap",
            get(|| async {
                Json(json!({ "rootContainers": [
                    { "id": "config", "entityTypeId": 1002 },
                    { "id": "work", "entityTypeId": 1000 },
                ] }))
            }),
        )
        .route(
            "/company/co-1/settings",
            get(|| async {
                Json(json!({ "attributes": {
                    "designer": { "edgeStyle": { "defaultEdgeStyleSet": "edges-1" } },
                    "job": { "defaultStagingLocationId": "indy" },
                } }))
            }),
        )
        .route(
            "/company/co-1/jobs",
            get(|| async {
                Json(json!({ "jobs": [
                    { "id": "old", "name": "Doe Jane Kitchen", "reference": "#341", "status": "Active" },
                    { "id": "gone", "name": "Builder Erin Kitchen", "reference": "#342", "status": "Deleted" },
                ] }))
            }),
        )
        .route(
            "/job/work/new",
            post(|State(fake): State<Fake>, Json(body): Json<Value>| async move {
                fake.log("POST", "/job/work/new".into(), body);
                let n = fake.next_job.fetch_add(1, Ordering::SeqCst) + 1;
                Json(json!({ "status": "Success", "uuid": format!("job-{n}") }))
            }),
        )
        .route(
            "/job/{id}",
            get(|Path(id): Path<String>| async move {
                Json(json!([
                    { "id": id, "entityTypeId": 400, "attributes": {} },
                    { "id": format!("info-{id}"), "entityTypeId": 404, "parentId": id, "attributes": {
                        "account": { "name": "Account", "id": "a", "properties": {} },
                        "customer": { "name": "Customer", "id": "c", "properties": {} },
                        "jobInfo": { "name": "JobInfo", "id": "j", "properties": {} },
                        "checklist": { "name": "Checklist", "id": "k", "properties": {} },
                        "categories": {},
                    } },
                ]))
            }),
        )
        .route(
            "/entity/{id}",
            put(
                |State(fake): State<Fake>, Path(id): Path<String>, Json(body): Json<Value>| async move {
                    for (_, note) in fake.notes.lock().unwrap().iter_mut() {
                        if note["id"] == id.as_str() {
                            *note = body.clone();
                        }
                    }
                    fake.log("PUT", format!("/entity/{id}"), body);
                    Json(json!({ "status": "Success" }))
                },
            ),
        )
        .route(
            "/entity/{id}/status",
            put(
                |State(fake): State<Fake>, Path(id): Path<String>, Json(body): Json<Value>| async move {
                    if fake.delete_fails.load(Ordering::SeqCst) {
                        return Err(StatusCode::INTERNAL_SERVER_ERROR);
                    }
                    fake.log("PUT", format!("/entity/{id}/status"), body);
                    Ok(Json(json!({ "status": "Success" })))
                },
            ),
        )
        .route(
            "/job/{id}/note",
            post(
                |State(fake): State<Fake>, Path(id): Path<String>, Json(body): Json<Value>| async move {
                    if fake.note_fails.load(Ordering::SeqCst) {
                        return Err(StatusCode::INTERNAL_SERVER_ERROR);
                    }
                    fake.notes.lock().unwrap().push((id.clone(), body.clone()));
                    fake.log("POST", format!("/job/{id}/note"), body);
                    Ok(Json(json!({ "status": "Success" })))
                },
            )
            .get(|State(fake): State<Fake>, Path(id): Path<String>| async move {
                let notes: Vec<Value> = fake
                    .notes
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(job, _)| *job == id)
                    .map(|(_, note)| note.clone())
                    .collect();
                Json(Value::Array(notes))
            }),
        )
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, fake)
}

async fn sql(pool: &MySqlPool, sql: &str) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
        .execute(pool)
        .await
        .unwrap();
}

/// Erin's sold quote on deal 8002 and the won deal's project.
async fn sold_project(pool: &MySqlPool) -> i32 {
    sql(
        pool,
        r#"UPDATE company SET auto_project = 1 WHERE id = 100;
           UPDATE customers SET phone = '317-555-0100', company_name = NULL WHERE id = 1004;
           INSERT INTO quotes (id, customer_id, deal_id, quote_name, quote_type, created_date, sales_rep,
                               company_id, salesperson_id, created_by, status, address, current_revision)
           VALUES (9901, 1004, 8002, 'Erin countertops', 'quote', NOW(), 'Rita Rep', 100, 100, 100,
                   'converted', '12 Oak St, Carmel, IN 46032', 1);
           INSERT INTO quote_revisions (quote_id, revision_number, drawing_json, created_by)
           VALUES (9901, 1, '{"version": 2,
               "areas": [{"id": "a1", "name": "Kitchen", "quantity": 1}],
               "colorOptions": [{"id": "o1", "byArea": {"a1": {"product": "Quartz", "color": "Calacatta", "edges": ["Eased"]}}}],
               "counters": [{"id": "c1", "areaId": "a1", "x": 0, "y": 0,
                   "runs": [{"length": 120, "depth": 25.5, "cutouts": [
                       {"id": "s1", "kind": "sink", "width": 33, "depth": 18, "sinkModel": "Kohler K-8199"}]}],
                   "edges": [{"type": "splash", "splashHeight": 4}, {"type": "finished"}, {"type": "finished"}, {"type": "finished"}],
                   "corners": [[null, null], [null, null]]}],
               "items": [{"id": "i1", "kind": "priceList", "name": "10 year Sealer"}]}', 100);"#,
    )
    .await;
    server(pool.clone())
        .put("/v1/deals/8002/outcome")
        .with_session(sessions::ADMIN)
        .json(&json!({ "outcome": "won" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    sqlx::query_scalar("SELECT id FROM projects WHERE source_deal_id = 8002")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn connect_raptor(pool: &MySqlPool, password: &str) {
    sqlx::query(
        "UPDATE company SET raptor_email = 'sales@acme.example', raptor_password = ?,
                raptor_since = NOW() - INTERVAL 1 HOUR WHERE id = 100",
    )
    .bind(::common::telephony::ringcentral_auth::encrypt(
        &KEY, password,
    ))
    .execute(pool)
    .await
    .unwrap();
}

async fn row(pool: &MySqlPool, project: i32) -> (String, i32, Value, Option<String>) {
    let (status, attempts, jobs, error): (String, i32, Option<String>, Option<String>) =
        sqlx::query_as(
            "SELECT status, attempts, CAST(jobs AS CHAR), error
               FROM project_raptor_jobs WHERE project_id = ?",
        )
        .bind(project)
        .fetch_one(pool)
        .await
        .unwrap();
    let jobs = jobs.map_or(Value::Null, |j| serde_json::from_str(&j).unwrap());
    (status, attempts, jobs, error)
}

fn config(url: &str) -> Config {
    Config {
        api_url: url.to_string(),
        key: Some(KEY),
        sale_companies: vec![],
    }
}

#[test_db::test(seed)]
async fn a_new_project_becomes_a_filled_raptor_job_once(pool: MySqlPool) {
    let project = sold_project(&pool).await;
    let (url, fake) = fake_raptor().await;

    // No Raptor login: nothing is sent.
    let summary = run(&pool, Duration::from_secs(60), &config(&url))
        .await
        .unwrap();
    assert_eq!((summary.done, summary.failed), (0, 0));
    assert!(fake.calls("POST", "/job").is_empty());

    connect_raptor(&pool, "secret").await;
    let summary = run(&pool, Duration::from_secs(60), &config(&url))
        .await
        .unwrap();
    assert_eq!((summary.done, summary.failed), (1, 0));

    // Next number after #342 (a deleted job still holds its number); the
    // deleted job's name is free again.
    let created = fake.calls("POST", "/job/work/new");
    assert_eq!(created.len(), 1);
    let body = &created[0].1;
    assert_eq!(body["name"], "Builder Erin Kitchen");
    assert_eq!(body["reference"], "#343");
    assert_eq!(body["jobStatusId"], 101);
    assert_eq!(body["edgeStyleSetId"], "edges-1");
    assert_eq!(body["stagingAddressId"], "indy");

    let saved = fake.calls("PUT", "/entity/info-job-1");
    assert_eq!(saved.len(), 1);
    let info = &saved[0].1["attributes"];
    assert_eq!(info["account"]["id"], "a");
    assert_eq!(
        info["customer"]["properties"]["name"]["value"],
        "Erin Builder"
    );
    assert_eq!(
        info["customer"]["properties"]["phone"]["value"],
        "317-555-0100"
    );
    let address = &info["jobInfo"]["properties"]["address"]["value"];
    assert_eq!(
        (
            &address["addressLine1"],
            &address["locality"],
            &address["region"],
            &address["postalCode"]
        ),
        (
            &json!("12 Oak St"),
            &json!("Carmel"),
            &json!("IN"),
            &json!("46032")
        )
    );
    let material = &info["categories"]["material"][0]["properties"];
    assert_eq!(material["brand"]["value"], "Quartz");
    assert_eq!(material["color"]["value"], "Calacatta");
    assert_eq!(
        material["edgeStyleSelection"]["value"]["primaryEdgeName"]["value"],
        "Eased"
    );
    let cutout = &info["categories"]["cutout"][0]["properties"]["cutoutType"]["value"];
    assert_eq!(cutout["model"]["value"], "Kohler K-8199");

    let notes = fake.calls("POST", "/job/job-1/note");
    assert_eq!(notes.len(), 1);
    let note = &notes[0].1["attributes"];
    assert_eq!(note["title"], "Notes");
    assert_eq!(note["workflowStageId"], 101);
    let text = note["body"].as_str().unwrap();
    assert!(text.starts_with("Floor:\nBacksplash:4\"\n"), "{text}");
    assert!(text.contains("\n10 year sealer:Yes\n"), "{text}");
    assert!(text.contains("\nSalesperson:Rita\n"), "{text}");

    let (status, attempts, jobs, error) = row(&pool, project).await;
    assert_eq!((status.as_str(), attempts, error), ("done", 1, None));
    assert_eq!(jobs[0]["reference"], "#343");

    // The next run sends nothing again.
    let summary = run(&pool, Duration::from_secs(60), &config(&url))
        .await
        .unwrap();
    assert_eq!((summary.done, summary.failed), (0, 0));
    assert_eq!(fake.calls("POST", "/job/work/new").len(), 1);
}

#[test_db::test(seed)]
async fn a_failed_project_carries_on_without_a_second_job(pool: MySqlPool) {
    let project = sold_project(&pool).await;
    connect_raptor(&pool, "secret").await;
    let (url, fake) = fake_raptor().await;
    fake.note_fails.store(true, Ordering::SeqCst);

    let summary = run(&pool, Duration::from_secs(60), &config(&url))
        .await
        .unwrap();
    assert_eq!((summary.done, summary.failed), (0, 1));
    let (status, attempts, jobs, error) = row(&pool, project).await;
    assert_eq!((status.as_str(), attempts), ("failed", 1));
    assert!(error.unwrap().contains("add note"));
    assert_eq!(jobs[0]["filled"], true);
    assert_eq!(jobs[0]["noted"], false);

    // Within 10 minutes it waits.
    fake.note_fails.store(false, Ordering::SeqCst);
    let summary = run(&pool, Duration::from_secs(60), &config(&url))
        .await
        .unwrap();
    assert_eq!((summary.done, summary.failed), (0, 0));

    // Later it adds only the note to the job it made.
    sql(
        &pool,
        "UPDATE project_raptor_jobs SET updated_at = NOW() - INTERVAL 11 MINUTE",
    )
    .await;
    let summary = run(&pool, Duration::from_secs(60), &config(&url))
        .await
        .unwrap();
    assert_eq!((summary.done, summary.failed), (1, 0));
    assert_eq!(fake.calls("POST", "/job/work/new").len(), 1);
    assert_eq!(fake.calls("PUT", "/entity/").len(), 1);
    assert_eq!(fake.calls("POST", "/job/job-1/note").len(), 1);
    let (status, attempts, _, _) = row(&pool, project).await;
    assert_eq!((status.as_str(), attempts), ("done", 2));
}

#[test_db::test(seed)]
async fn older_projects_and_jobs_made_by_hand_are_left_alone(pool: MySqlPool) {
    let project = sold_project(&pool).await;
    let (url, fake) = fake_raptor().await;

    // Projects made before the login was saved are never sent.
    connect_raptor(&pool, "secret").await;
    sql(
        &pool,
        "UPDATE company SET raptor_since = NOW() + INTERVAL 1 HOUR WHERE id = 100",
    )
    .await;
    run(&pool, Duration::from_secs(60), &config(&url))
        .await
        .unwrap();
    assert!(fake.calls("POST", "/job").is_empty());

    // A job the office already made under that name is kept, not doubled.
    connect_raptor(&pool, "secret").await;
    sql(
        &pool,
        "UPDATE customers SET name = 'Jane Doe' WHERE id = 1004",
    )
    .await;
    let summary = run(&pool, Duration::from_secs(60), &config(&url))
        .await
        .unwrap();
    assert_eq!((summary.done, summary.failed), (1, 0));
    assert!(fake.calls("POST", "/job").is_empty());
    let (status, _, jobs, _) = row(&pool, project).await;
    assert_eq!(status, "done");
    assert_eq!(jobs[0]["existing"], true);
    assert_eq!(jobs[0]["reference"], "#341");
}

#[test_db::test(seed)]
async fn a_wrong_password_uses_no_attempt(pool: MySqlPool) {
    let project = sold_project(&pool).await;
    connect_raptor(&pool, "wrong").await;
    let (url, fake) = fake_raptor().await;
    let summary = run(&pool, Duration::from_secs(60), &config(&url))
        .await
        .unwrap();
    assert_eq!((summary.done, summary.failed), (0, 0));
    assert!(fake.calls("POST", "/job").is_empty());
    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM project_raptor_jobs WHERE project_id = ?")
            .bind(project)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 0);
}

/// The next look at the project's jobs is due (15 minutes later).
async fn later(pool: &MySqlPool, project: i32) {
    sqlx::query(
        "UPDATE project_raptor_jobs SET updated_at = NOW() - INTERVAL 20 MINUTE WHERE project_id = ?",
    )
    .bind(project)
    .execute(pool)
    .await
    .unwrap();
}

#[test_db::test(seed)]
async fn a_changed_quote_updates_the_job_and_keeps_the_office_lines(pool: MySqlPool) {
    let project = sold_project(&pool).await;
    connect_raptor(&pool, "secret").await;
    let (url, fake) = fake_raptor().await;
    run(&pool, Duration::from_secs(60), &config(&url))
        .await
        .unwrap();
    assert_eq!(fake.calls("PUT", "/entity/").len(), 1);

    // Nothing changed: nothing is written.
    later(&pool, project).await;
    let summary = run(&pool, Duration::from_secs(60), &config(&url))
        .await
        .unwrap();
    assert_eq!(summary.updated, 0);
    assert_eq!(fake.calls("PUT", "/entity/").len(), 1);

    // The office types a floor; the color changes and the layout is known.
    {
        let mut notes = fake.notes.lock().unwrap();
        let body = notes[0].1["attributes"]["body"]
            .as_str()
            .unwrap()
            .replace("Floor:\n", "Floor:2nd\n");
        notes[0].1["attributes"]["body"] = json!(body);
    }
    sql(
        &pool,
        &format!(
            r#"UPDATE quote_revisions SET drawing_json = JSON_SET(drawing_json, '$.colorOptions[0].byArea.a1.color', 'Statuario')
                WHERE quote_id = 9901;
               INSERT INTO project_notes (project_id, company_id, content, created_by)
               VALUES ({project}, 100, 'Layout yes', 100);"#
        ),
    )
    .await;
    later(&pool, project).await;
    let summary = run(&pool, Duration::from_secs(60), &config(&url))
        .await
        .unwrap();
    assert_eq!(summary.updated, 1);
    assert!(
        fake.calls("POST", "/job/work/new").len() == 1,
        "no second job"
    );

    // Only the materials of Job Info are written again.
    let saved = fake.calls("PUT", "/entity/info-job-1");
    assert_eq!(saved.len(), 2);
    let info = &saved[1].1["attributes"];
    assert_eq!(
        info["categories"]["material"][0]["properties"]["color"]["value"],
        "Statuario"
    );
    assert!(info["categories"].get("cutout").is_none());
    assert_eq!(info["customer"]["properties"], json!({}));
    assert!(fake.calls("PUT", "/entity/job-1").is_empty(), "same name");

    // The note keeps the office's floor and gets the layout.
    let notes = fake.notes.lock().unwrap().clone();
    assert_eq!(notes.len(), 1);
    let text = notes[0].1["attributes"]["body"].as_str().unwrap();
    assert!(text.starts_with("Floor:2nd\n"), "{text}");
    assert!(text.contains("\nLayout:Yes\n"), "{text}");

    // Done once: the next look writes nothing.
    later(&pool, project).await;
    let summary = run(&pool, Duration::from_secs(60), &config(&url))
        .await
        .unwrap();
    assert_eq!(summary.updated, 0);
    assert_eq!(fake.calls("PUT", "/entity/").len(), 3);
}

#[test_db::test(seed)]
async fn a_sale_company_follows_mark_as_sold_and_undo_sale(pool: MySqlPool) {
    let project = sold_project(&pool).await;
    // The project is older than the Raptor login.
    sqlx::query("UPDATE projects SET created_at = NOW() - INTERVAL 30 DAY WHERE id = ?")
        .bind(project)
        .execute(&pool)
        .await
        .unwrap();
    connect_raptor(&pool, "secret").await;
    let (url, fake) = fake_raptor().await;
    let sales = Config {
        sale_companies: vec![100],
        ..config(&url)
    };
    let go = || async { run(&pool, Duration::from_secs(60), &sales).await.unwrap() };
    let created = || fake.calls("POST", "/job/work/new");
    let deleted = || fake.calls("PUT", "/entity/job-");

    // A project alone sends nothing: Mark as Sold queues it.
    assert_eq!(go().await.done, 0);
    sold(&pool, &sales, 100, 8002).await;
    assert_eq!(row(&pool, project).await.0, "waiting");
    assert_eq!(go().await.done, 1);
    assert_eq!(created().len(), 1);
    // A TEST job that keeps the office's numbers free.
    assert_eq!(created()[0].1["name"], "Builder Erin Kitchen TEST");
    assert_eq!(created()[0].1["reference"], "TEST");

    // Sold again while it is there: nothing new.
    sold(&pool, &sales, 100, 8002).await;
    assert_eq!(row(&pool, project).await.0, "done");
    assert_eq!(go().await.done, 0);

    // Undo sale waits for no one; the next run deletes the job and forgets it.
    sql(&pool, "UPDATE quotes SET status = 'active' WHERE id = 9901").await;
    unsold(&pool, &sales, 100, Some(8002)).await;
    assert_eq!(row(&pool, project).await.0, "removing");
    assert!(deleted().is_empty());
    assert_eq!(go().await.removed, 1);
    assert_eq!(deleted().len(), 1);
    assert_eq!(deleted()[0].0, "/entity/job-1/status");
    assert_eq!(
        deleted()[0].1,
        json!({ "status": 10, "setChildren": false })
    );
    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM project_raptor_jobs WHERE project_id = ?")
            .bind(project)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 0);
    assert_eq!(created().len(), 1);

    // Sold again: a new job.
    sql(
        &pool,
        "UPDATE quotes SET status = 'converted' WHERE id = 9901",
    )
    .await;
    sold(&pool, &sales, 100, 8002).await;
    assert_eq!(go().await.done, 1);
    assert_eq!(created().len(), 2);

    // Raptor is down at Undo sale: the job stays marked and is tried again.
    fake.delete_fails.store(true, Ordering::SeqCst);
    unsold(&pool, &sales, 100, Some(8002)).await;
    assert_eq!(go().await.removed, 0);
    let (status, attempts, _, error) = row(&pool, project).await;
    assert_eq!((status.as_str(), attempts), ("removing", 1));
    assert!(error.unwrap().contains("delete job"));
    assert_eq!(go().await.removed, 0, "not again within 10 minutes");
    fake.delete_fails.store(false, Ordering::SeqCst);
    sqlx::query(
        "UPDATE project_raptor_jobs SET updated_at = NOW() - INTERVAL 11 MINUTE WHERE project_id = ?",
    )
    .bind(project)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(go().await.removed, 1);
    assert_eq!(deleted().len(), 2);

    // Sold while its job is still marked: the job is kept, none is added.
    sold(&pool, &sales, 100, 8002).await;
    assert_eq!(go().await.done, 1);
    assert_eq!(created().len(), 3);
    unsold(&pool, &sales, 100, Some(8002)).await;
    sold(&pool, &sales, 100, 8002).await;
    assert_eq!(row(&pool, project).await.0, "waiting");
    assert_eq!(go().await.done, 1);
    assert_eq!((created().len(), deleted().len()), (3, 2));

    // A project that gave up starts over on the next Mark as Sold.
    sqlx::query(
        "UPDATE project_raptor_jobs SET status = 'failed', attempts = 3, error = 'x' WHERE project_id = ?",
    )
    .bind(project)
    .execute(&pool)
    .await
    .unwrap();
    sold(&pool, &sales, 100, 8002).await;
    let (status, attempts, _, error) = row(&pool, project).await;
    assert_eq!((status.as_str(), attempts, error), ("waiting", 0, None));

    // Another company's buttons change nothing.
    unsold(&pool, &config(&url), 100, Some(8002)).await;
    assert_eq!(row(&pool, project).await.0, "waiting");
}
