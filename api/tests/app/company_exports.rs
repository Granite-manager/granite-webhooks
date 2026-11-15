//! The daily company data export (`POST /v1/jobs/company-exports`).

use crate::common::{ALLOWED_ORIGIN, SECRET};
use api::routes::jobs::company_exports::archive;
use api::routes::jobs::company_exports::tables::{
    self, FILE_COLUMNS, FileColumn, NOT_FILE_COLUMNS, Rule, SECRET_COLUMNS, Scope, TABLES,
};
use api::routes::jobs::company_exports::{Mailer, SentMail, run_due};
use api::state::AppState;
use api::storage::Storage;
use chrono::{DateTime, TimeZone, Utc};
use sqlx::MySqlPool;
use std::collections::{BTreeMap, HashMap};
use std::io::Read as _;
use std::sync::{Arc, Mutex};

const BUCKET: &str = "private-exports";

fn state(pool: &MySqlPool) -> (AppState, Storage, Arc<Mutex<Vec<SentMail>>>) {
    let storage = Storage::memory();
    let sent = Arc::new(Mutex::new(Vec::new()));
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_signature_bucket(BUCKET)
        .with_storage(storage.clone())
        .with_extension(Mailer::Memory(sent.clone()));
    (state, storage, sent)
}

/// `hour`:30 New York time on `day` of November 2026 (EST, UTC-5).
fn new_york(day: u32, hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, day, hour + 5, 30, 0)
        .unwrap()
}

async fn set_up(pool: &MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO company_exports (company_id, recipients, alert_recipients)
              VALUES (100, 'owner@acme.test, Дмитрий Заика <dima@acme.test>', 'ops@acme.test');
         INSERT INTO images (id, name, url, company_id) VALUES
           (9100, 'Kitchen', 'https://files.test/bucket/dynamic-images/images/kitchen.jpg', 100),
           (9101, 'Theirs', 'https://files.test/bucket/dynamic-images/images/theirs.jpg', 101),
           (9102, 'Logo link', 'https://example.test/logo.png', 100);",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn put(storage: &Storage, key: &str, bytes: &[u8]) {
    storage
        .put_private(BUCKET, key, bytes.to_vec(), "image/jpeg")
        .await
        .unwrap();
}

/// The zips stored so far, by key.
fn zips(storage: &Storage) -> Vec<String> {
    storage
        .memory_keys()
        .into_iter()
        .filter(|key| key.starts_with("company-exports/"))
        .collect()
}

struct Zip(BTreeMap<String, Vec<u8>>);

impl Zip {
    async fn read(storage: &Storage, key: &str) -> Self {
        let bytes = storage.get(key).await.unwrap().unwrap().bytes;
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut files = BTreeMap::new();
        for index in 0..archive.len() {
            let mut file = archive.by_index(index).unwrap();
            let mut content = Vec::new();
            file.read_to_end(&mut content).unwrap();
            files.insert(file.name().to_string(), content);
        }
        Self(files)
    }

    fn text(&self, name: &str) -> String {
        let bytes = self
            .0
            .get(name)
            .unwrap_or_else(|| panic!("{name} in the zip"));
        String::from_utf8(
            bytes
                .strip_prefix(b"\xEF\xBB\xBF")
                .unwrap_or(bytes)
                .to_vec(),
        )
        .unwrap()
    }

    /// A CSV's header and rows.
    fn csv(&self, table: &str) -> (Vec<String>, Vec<Vec<Option<String>>>) {
        let mut lines = parse_csv(&self.text(&format!("data/{table}.csv")));
        let header = lines.remove(0).into_iter().map(Option::unwrap).collect();
        (header, lines)
    }
}

/// RFC 4180 with the export's NULL convention (empty field = NULL).
fn parse_csv(text: &str) -> Vec<Vec<Option<String>>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut was_quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => quoted = false,
                _ => field.push(c),
            }
            continue;
        }
        match c {
            '"' => {
                quoted = true;
                was_quoted = true;
            }
            ',' | '\r' => {
                if c == ',' || chars.peek() == Some(&'\n') {
                    if c == '\r' {
                        chars.next();
                    }
                    let value = std::mem::take(&mut field);
                    row.push((was_quoted || !value.is_empty()).then_some(value));
                    was_quoted = false;
                    if c == '\r' {
                        rows.push(std::mem::take(&mut row));
                    }
                } else {
                    field.push(c);
                }
            }
            _ => field.push(c),
        }
    }
    rows
}

#[test_db::test]
async fn every_table_and_column_of_the_database_is_classified(pool: MySqlPool) {
    let schema = archive::schema(&pool).await.unwrap();
    let mut problems = Vec::new();
    let has = |table: &str, column: &str| {
        schema
            .get(table)
            .is_some_and(|columns| columns.iter().any(|c| c.name == column))
    };
    for table in schema.keys() {
        if tables::table(table).is_none() {
            problems.push(format!(
                "table {table} is not in company_exports/tables.rs TABLES: export or skip it"
            ));
        }
    }
    for table in TABLES {
        if !schema.contains_key(table.name) {
            if tables::PRODUCTION_ONLY_TABLES.contains(&table.name) {
                continue;
            }
            problems.push(format!(
                "TABLES lists {} but the database has no such table",
                table.name
            ));
            continue;
        }
        match table.rule {
            Rule::Export(Scope::Company) if !has(table.name, "company_id") => {
                problems.push(format!("{} has no company_id", table.name));
            }
            Rule::Export(Scope::Parent(column, parent, parent_column)) => {
                if !has(table.name, column) {
                    problems.push(format!("{}.{column} does not exist", table.name));
                }
                if !has(parent, parent_column) {
                    problems.push(format!("{parent}.{parent_column} does not exist"));
                }
            }
            _ => {}
        }
        if let Rule::Export(_) = table.rule {
            for column in &schema[table.name] {
                let known_file = FILE_COLUMNS.iter().any(|file| {
                    let (FileColumn::Url(t, c) | FileColumn::Key(t, c, ..)) = file;
                    *t == table.name && *c == column.name
                }) || NOT_FILE_COLUMNS
                    .contains(&(table.name, column.name.as_str()));
                if tables::looks_like_file_column(&column.name)
                    && !known_file
                    && !tables::column_is_left_out(table.name, &column.name)
                {
                    problems.push(format!(
                        "{}.{} looks like a file: list it in FILE_COLUMNS or NOT_FILE_COLUMNS",
                        table.name, column.name
                    ));
                }
            }
        }
    }
    for (table, column) in SECRET_COLUMNS {
        if !has(table, column) {
            problems.push(format!(
                "SECRET_COLUMNS lists {table}.{column}, which does not exist"
            ));
        }
    }
    for file in FILE_COLUMNS {
        let (FileColumn::Url(table, column) | FileColumn::Key(table, column, ..)) = file;
        if !has(table, column) {
            problems.push(format!(
                "FILE_COLUMNS lists {table}.{column}, which does not exist"
            ));
        }
    }
    // Each table's query runs.
    for table in TABLES {
        if tables::PRODUCTION_ONLY_TABLES.contains(&table.name) {
            continue;
        }
        if let Some((condition, binds)) = tables::condition(table.name) {
            let sql = format!("SELECT COUNT(*) FROM `{}` WHERE {condition}", table.name);
            let mut query = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql));
            for _ in 0..binds {
                query = query.bind(100);
            }
            if let Err(error) = query.fetch_one(&pool).await {
                problems.push(format!("{}: {error}", table.name));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test_db::test(seed)]
async fn the_export_holds_the_company_rows_and_files_and_nothing_else(pool: MySqlPool) {
    set_up(&pool).await;
    let (state, storage, sent) = state(&pool);
    put(
        &storage,
        "dynamic-images/images/kitchen.jpg",
        b"kitchen-bytes",
    )
    .await;
    put(
        &storage,
        "dynamic-images/images/theirs.jpg",
        b"theirs-bytes",
    )
    .await;

    // Before 6 AM New York time: nothing.
    let early = run_due(&state, new_york(10, 5)).await.unwrap();
    assert_eq!(early.sent, 0);
    assert!(zips(&storage).is_empty());

    let run = run_due(&state, new_york(10, 6)).await.unwrap();
    assert_eq!((run.sent, run.failed), (1, 0), "{run:?}");
    let keys = zips(&storage);
    assert_eq!(keys.len(), 1);
    assert!(
        keys[0].starts_with("company-exports/100/2026-11-10-"),
        "{keys:?}"
    );

    let mail = sent.lock().unwrap().clone();
    assert_eq!(mail.len(), 1);
    assert_eq!(
        mail[0].to,
        vec![
            "owner@acme.test".to_string(),
            "Дмитрий Заика <dima@acme.test>".to_string()
        ]
    );
    assert_eq!(
        mail[0].subject,
        "Acme Stone data export - November 10, 2026"
    );
    assert!(mail[0].html.contains(&keys[0]), "{}", mail[0].html);
    assert!(
        mail[0].html.contains("November 17, 2026"),
        "link expiry: {}",
        mail[0].html
    );

    let zip = Zip::read(&storage, &keys[0]).await;
    let manifest: serde_json::Value = serde_json::from_str(&zip.text("manifest.json")).unwrap();
    assert_eq!(manifest["company_id"], 100);
    assert_eq!(manifest["full_files"], true);
    assert_eq!(manifest["unknown_tables"], serde_json::json!([]));
    assert_eq!(
        manifest["missing_tables"],
        serde_json::json!(
            TABLES
                .iter()
                .filter(|t| matches!(t.rule, Rule::Export(_))
                    && tables::PRODUCTION_ONLY_TABLES.contains(&t.name))
                .map(|t| t.name)
                .collect::<Vec<_>>()
        )
    );

    // Every exported table's rows: the count the database has, and no
    // other company's row.
    let mut checked = 0;
    let mut total = 0;
    for table in TABLES {
        let Some((condition, binds)) = tables::condition(table.name) else {
            assert!(
                !zip.0.contains_key(&format!("data/{}.csv", table.name)),
                "{}",
                table.name
            );
            continue;
        };
        if tables::PRODUCTION_ONLY_TABLES.contains(&table.name) {
            continue;
        }
        let sql = format!("SELECT COUNT(*) FROM `{}` WHERE {condition}", table.name);
        let mut query = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql));
        for _ in 0..binds {
            query = query.bind(100);
        }
        let expected = usize::try_from(query.fetch_one(&pool).await.unwrap()).unwrap();
        let (header, rows) = zip.csv(table.name);
        assert_eq!(rows.len(), expected, "{}", table.name);
        assert!(
            rows.iter().all(|row| row.len() == header.len()),
            "{} row width",
            table.name
        );
        let entry = manifest["tables"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["table"] == table.name)
            .unwrap();
        assert_eq!(entry["rows"], expected, "{}", table.name);
        total += expected;
        for column in &header {
            assert!(
                !tables::column_is_left_out(table.name, column),
                "{}.{column}",
                table.name
            );
        }
        if let Some(at) = header.iter().position(|column| column == "company_id") {
            for row in &rows {
                assert_eq!(
                    row[at].as_deref(),
                    Some("100"),
                    "{} row of another company",
                    table.name
                );
            }
        }
        if table.rule == Rule::Export(Scope::Company) {
            let independent: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT COUNT(*) FROM `{}` WHERE company_id = 100",
                table.name
            )))
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(
                rows.len(),
                usize::try_from(independent).unwrap(),
                "{}",
                table.name
            );
        }
        checked += 1;
    }
    assert!(checked > 200, "{checked} tables");
    assert_eq!(manifest["row_count"], total);

    // Hand-checked tables.
    let (header, rows) = zip.csv("users");
    assert!(!header.contains(&"password".to_string()));
    assert!(header.contains(&"email".to_string()));
    let customers: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM customers WHERE company_id = 100")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(customers > 0);
    assert_eq!(
        zip.csv("customers").1.len(),
        usize::try_from(customers).unwrap()
    );
    let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE company_id = 100")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), usize::try_from(users).unwrap());
    let (header, rows) = zip.csv("company");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0][header.iter().position(|c| c == "name").unwrap()].as_deref(),
        Some("Acme Stone")
    );
    for secret in [
        "qbo_client_secret",
        "qbo_refresh_token",
        "ringcentral_jwt",
        "signwell_api_key",
        "raptor_password",
    ] {
        assert!(!header.contains(&secret.to_string()), "{secret}");
    }
    for skipped in [
        "sessions",
        "access_tokens",
        "device_sessions",
        "phone_devices",
        "password_reset_tokens",
    ] {
        assert!(
            !zip.0.contains_key(&format!("data/{skipped}.csv")),
            "{skipped}"
        );
    }
    let deals_of_others: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM deals d JOIN customers c ON c.id = d.customer_id WHERE c.company_id <> 100",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(deals_of_others > 0, "the seed has another company's deals");
    let (header, rows) = zip.csv("deals");
    let customer_at = header.iter().position(|c| c == "customer_id").unwrap();
    let ours: Vec<String> =
        sqlx::query_scalar("SELECT CAST(id AS CHAR) FROM customers WHERE company_id = 100")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(
        rows.iter()
            .all(|row| ours.contains(row[customer_at].as_ref().unwrap()))
    );

    // The files: ours in the zip, theirs not, links listed.
    assert_eq!(
        zip.0["files/main/dynamic-images/images/kitchen.jpg"],
        b"kitchen-bytes"
    );
    assert!(!zip.0.keys().any(|name| name.contains("theirs")));
    let index = parse_csv(&zip.text("files.csv"));
    let statuses: HashMap<String, String> = index[1..]
        .iter()
        .map(|row| (row[3].clone().unwrap(), row[5].clone().unwrap()))
        .collect();
    assert_eq!(
        statuses["https://files.test/bucket/dynamic-images/images/kitchen.jpg"],
        "included"
    );
    assert_eq!(
        statuses["https://example.test/logo.png"],
        "external link (not copied)"
    );
    assert!(!statuses.keys().any(|value| value.contains("theirs")));
    assert!(zip.text("README.txt").contains("Acme Stone"));

    // Once a day.
    let again = run_due(&state, new_york(10, 7)).await.unwrap();
    assert_eq!(again.sent, 0);
    assert_eq!(sent.lock().unwrap().len(), 1);
    assert_eq!(zips(&storage).len(), 1);
}

#[test_db::test(seed)]
async fn the_next_days_add_only_new_files_and_mondays_all(pool: MySqlPool) {
    set_up(&pool).await;
    let (state, storage, sent) = state(&pool);
    put(
        &storage,
        "dynamic-images/images/kitchen.jpg",
        b"kitchen-bytes",
    )
    .await;
    run_due(&state, new_york(10, 6)).await.unwrap();

    sqlx::query(
        "INSERT INTO images (id, name, url, company_id) VALUES
           (9103, 'Bath', 'https://files.test/bucket/dynamic-images/images/bath.jpg', 100)",
    )
    .execute(&pool)
    .await
    .unwrap();
    put(&storage, "dynamic-images/images/bath.jpg", b"bath-bytes").await;
    // Wednesday: only the new picture.
    let run = run_due(&state, new_york(11, 8)).await.unwrap();
    assert_eq!(run.sent, 1);
    let key = zips(&storage)
        .into_iter()
        .find(|key| key.contains("2026-11-11"))
        .unwrap();
    let zip = Zip::read(&storage, &key).await;
    assert_eq!(
        zip.0["files/main/dynamic-images/images/bath.jpg"],
        b"bath-bytes"
    );
    assert!(
        !zip.0
            .contains_key("files/main/dynamic-images/images/kitchen.jpg")
    );
    let index = zip.text("files.csv");
    assert!(
        index.contains(
            "kitchen.jpg,files/main/dynamic-images/images/kitchen.jpg,sent in an earlier export"
        ),
        "{index}"
    );
    let manifest: serde_json::Value = serde_json::from_str(&zip.text("manifest.json")).unwrap();
    assert_eq!(manifest["full_files"], false);
    assert_eq!(manifest["files"]["included"], 1);
    assert_eq!(manifest["files"]["sent_before"], 1);

    // Monday 16th: every file again.
    run_due(&state, new_york(16, 6)).await.unwrap();
    let key = zips(&storage)
        .into_iter()
        .find(|key| key.contains("2026-11-16"))
        .unwrap();
    let zip = Zip::read(&storage, &key).await;
    assert!(
        zip.0
            .contains_key("files/main/dynamic-images/images/kitchen.jpg")
    );
    assert!(
        zip.0
            .contains_key("files/main/dynamic-images/images/bath.jpg")
    );
    assert_eq!(sent.lock().unwrap().len(), 3);
}

#[test_db::test(seed)]
async fn a_failing_day_is_tried_three_times_then_alerted_once(pool: MySqlPool) {
    set_up(&pool).await;
    let sent = Arc::new(Mutex::new(Vec::new()));
    // No private bucket: every attempt fails.
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_storage(Storage::memory())
        .with_extension(Mailer::Memory(sent.clone()));
    let attempts = || async {
        sqlx::query_as::<_, (String, i32, Option<String>)>(
            "SELECT CAST(status AS CHAR), CAST(attempts AS SIGNED), error FROM company_export_runs WHERE company_id = 100",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let first = run_due(&state, new_york(10, 6)).await.unwrap();
    assert_eq!((first.failed, first.alerts), (1, 0));
    let (status, count, error) = attempts().await;
    assert_eq!((status.as_str(), count), ("failed", 1));
    assert!(error.unwrap().contains("private bucket"));
    // Not again within five minutes.
    assert_eq!(run_due(&state, new_york(10, 6)).await.unwrap().failed, 0);
    for attempt in 2..=3 {
        sqlx::query(
            "UPDATE company_export_runs SET finished_at = finished_at - INTERVAL 10 MINUTE",
        )
        .execute(&pool)
        .await
        .unwrap();
        let run = run_due(&state, new_york(10, 6)).await.unwrap();
        assert_eq!(run.failed, 1, "attempt {attempt}");
        assert_eq!(attempts().await.1, attempt);
        assert_eq!(run.alerts, usize::from(attempt == 3));
    }
    sqlx::query("UPDATE company_export_runs SET finished_at = finished_at - INTERVAL 10 MINUTE")
        .execute(&pool)
        .await
        .unwrap();
    let after = run_due(&state, new_york(10, 9)).await.unwrap();
    assert_eq!((after.failed, after.alerts), (0, 0));
    let mail = sent.lock().unwrap().clone();
    assert_eq!(mail.len(), 1, "{mail:?}");
    assert_eq!(mail[0].to, vec!["ops@acme.test".to_string()]);
    assert!(mail[0].subject.contains("failed"));
}

#[test_db::test(seed)]
async fn a_run_that_died_is_taken_over(pool: MySqlPool) {
    set_up(&pool).await;
    let (state, _storage, sent) = state(&pool);
    sqlx::query(
        "INSERT INTO company_export_runs (company_id, export_date, status, attempts, started_at)
         VALUES (100, '2026-11-10', 'running', 1, UTC_TIMESTAMP())",
    )
    .execute(&pool)
    .await
    .unwrap();
    // Still running: left alone.
    assert_eq!(run_due(&state, new_york(10, 6)).await.unwrap().sent, 0);
    sqlx::query("UPDATE company_export_runs SET started_at = started_at - INTERVAL 30 MINUTE")
        .execute(&pool)
        .await
        .unwrap();
    // Marked failed, then retried five minutes later.
    assert_eq!(run_due(&state, new_york(10, 6)).await.unwrap().sent, 0);
    sqlx::query("UPDATE company_export_runs SET finished_at = finished_at - INTERVAL 10 MINUTE")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(run_due(&state, new_york(10, 6)).await.unwrap().sent, 1);
    assert_eq!(sent.lock().unwrap().len(), 1);
}

#[test_db::test(seed)]
async fn zips_older_than_seven_days_are_deleted(pool: MySqlPool) {
    set_up(&pool).await;
    let (state, storage, _sent) = state(&pool);
    run_due(&state, new_york(10, 6)).await.unwrap();
    assert_eq!(zips(&storage).len(), 1);
    sqlx::query("UPDATE company_export_runs SET started_at = started_at - INTERVAL 6 DAY")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(run_due(&state, new_york(10, 9)).await.unwrap().deleted, 0);
    sqlx::query("UPDATE company_export_runs SET started_at = started_at - INTERVAL 2 DAY")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(run_due(&state, new_york(10, 9)).await.unwrap().deleted, 1);
    assert!(zips(&storage).is_empty());
    let deleted: Option<chrono::NaiveDateTime> =
        sqlx::query_scalar("SELECT deleted_at FROM company_export_runs WHERE company_id = 100")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(deleted.is_some());
}

#[test_db::test(seed)]
async fn the_job_route_needs_the_job_key(pool: MySqlPool) {
    let state = AppState::new(pool, SECRET, ALLOWED_ORIGIN).with_job_key("job-secret");
    let server = axum_test::TestServer::new(api::app(state));
    server
        .post("/v1/jobs/company-exports")
        .await
        .assert_status(axum::http::StatusCode::UNAUTHORIZED);
    let run: serde_json::Value = server
        .post("/v1/jobs/company-exports")
        .add_header("authorization", "job-secret")
        .await
        .json();
    assert_eq!(run["processed"], 0);
}
