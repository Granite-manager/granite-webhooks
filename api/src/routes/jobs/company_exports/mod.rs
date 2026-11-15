//! `POST /v1/jobs/company-exports`: the daily export of a company's data.
//!
//! Each company in `company_exports` gets, once a day from `send_hour` in its
//! `time_zone`, a zip of its rows (see [`tables`]) and its stored files (new
//! since the last export; all of them on Mondays), stored in the private
//! bucket under `company-exports/<company>/`, and an email to `recipients`
//! with a link that works `keep_days` (at most 7). The scheduler calls this
//! every minute: a day's run is a row of `company_export_runs` claimed with
//! its unique key, so it goes out once. A failed run is tried again a few
//! minutes later, three times in all; then `alert_recipients` are emailed.
//! Zips older than `keep_days` are deleted.

pub mod archive;
pub mod tables;

use crate::auth::job::JobCaller;
use crate::error::{ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Datelike, NaiveDate, Timelike, Utc, Weekday};
use chrono_tz::Tz;
use lambda_http::tracing;
use serde::Serialize;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// Attempts at one day's export before the alert goes out.
const MAX_ATTEMPTS: i32 = 3;
/// Wait after a failed attempt before the next.
const RETRY_AFTER_MINUTES: i32 = 5;
/// A run still "running" this long has died (a Lambda stops at 15 minutes).
const STALE_AFTER_MINUTES: i32 = 20;
/// The longest a presigned link can work.
const MAX_LINK_DAYS: i64 = 7;
/// Where the zips are, in the private bucket.
const PREFIX: &str = "company-exports";
const FROM: &str = "\"Granite Manager\" <noreply@granite-manager.com>";

/// What one call did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub struct ExportsRun {
    /// Exports sent (what the scheduler logs).
    pub processed: usize,
    /// Exports sent.
    pub sent: usize,
    /// Export attempts that failed (tried again a few minutes later).
    pub failed: usize,
    /// Failure alerts sent.
    pub alerts: usize,
    /// Old zips deleted from storage.
    pub deleted: usize,
}

/// How the export emails go out: SES, or a list for tests.
#[derive(Clone, Default)]
pub enum Mailer {
    #[default]
    Ses,
    Memory(Arc<Mutex<Vec<SentMail>>>),
}

/// An email the [`Mailer::Memory`] took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentMail {
    pub to: Vec<String>,
    pub subject: String,
    pub html: String,
}

impl Mailer {
    async fn send(&self, to: &[String], subject: &str, html: &str) -> Result<(), String> {
        match self {
            Self::Ses => {
                let to: Vec<String> = to.iter().map(|address| ses_address(address)).collect();
                let to: Vec<&str> = to.iter().map(String::as_str).collect();
                common::amazon::email::send_message_from(&to, subject, html, FROM)
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            }
            Self::Memory(sent) => {
                sent.lock().expect("lock").push(SentMail {
                    to: to.to_vec(),
                    subject: subject.to_string(),
                    html: html.to_string(),
                });
                Ok(())
            }
        }
    }
}

/// `Name <address>` as SES takes it: a name that is not ASCII in RFC 2047
/// encoded-word form.
fn ses_address(address: &str) -> String {
    use base64::Engine as _;
    let address = address.trim();
    if address.is_ascii() {
        return address.to_string();
    }
    match (address.rfind('<'), address.rfind('>')) {
        (Some(open), Some(close)) if close > open => {
            let name = address[..open].trim().trim_matches('"').trim();
            let email = address[open + 1..close].trim();
            if name.is_empty() {
                return email.to_string();
            }
            format!(
                "=?UTF-8?B?{}?= <{email}>",
                base64::engine::general_purpose::STANDARD.encode(name)
            )
        }
        _ => address.to_string(),
    }
}

/// A company's export settings (`company_exports`).
#[derive(Debug, Clone, sqlx::FromRow)]
struct Config {
    company_id: i32,
    recipients: String,
    alert_recipients: Option<String>,
    send_hour: i8,
    time_zone: String,
    keep_days: i16,
}

/// The addresses of a comma- or semicolon-separated list.
fn addresses(list: &str) -> Vec<String> {
    list.split([',', ';', '\n'])
        .map(str::trim)
        .filter(|address| address.contains('@'))
        .map(str::to_string)
        .collect()
}

/// Send today's company data exports
///
/// Called by the scheduler every minute. For each company with an export set
/// up, once its local time reaches the export hour, builds that day's zip
/// (every table's rows as CSV, new pictures and documents, all of them on
/// Mondays), stores it privately and emails the recipients a download link
/// that works 7 days; once per company and day. Failed exports are retried
/// three times, then an alert is emailed. Zips older than 7 days are deleted.
#[crate::openapi::path(
    post,
    path = "/v1/jobs/company-exports",
    operation_id = "run_company_exports",
    tag = "jobs",
    responses(
        (status = 200, description = "What was sent, retried, alerted and deleted", body = ExportsRun),
        (status = 401, description = "Missing or wrong job key", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("job_key" = []))
)]
pub async fn run(State(state): State<AppState>, _job: JobCaller) -> ApiResult<Json<ExportsRun>> {
    Ok(Json(run_due(&state, Utc::now()).await?))
}

/// Sends the exports due at `now` (the local hours and dates come from it;
/// retry delays from the database's clock).
pub async fn run_due(state: &AppState, now: DateTime<Utc>) -> Result<ExportsRun, sqlx::Error> {
    let mut run = ExportsRun {
        deleted: delete_old_zips(state).await?,
        ..ExportsRun::default()
    };
    let configs: Vec<Config> = sqlx::query_as(
        "SELECT company_id, recipients, alert_recipients, send_hour, time_zone, keep_days
           FROM company_exports WHERE is_enabled = 1 ORDER BY company_id",
    )
    .fetch_all(&state.pool)
    .await?;
    let mailer = state.extensions.get_or_default::<Mailer>();
    for config in configs {
        let Ok(zone) = config.time_zone.parse::<Tz>() else {
            tracing::error!(
                company_id = config.company_id,
                time_zone = %config.time_zone,
                "Company export has an unknown time zone"
            );
            continue;
        };
        let local = now.with_timezone(&zone);
        let day = local.date_naive();
        if i64::from(local.hour()) >= i64::from(config.send_hour)
            && let Some(claim) = claim(state, config.company_id, day).await?
        {
            match export_and_send(state, &mailer, &config, day, local, claim).await {
                Ok(()) => {
                    run.sent += 1;
                    run.processed += 1;
                }
                Err(error) => {
                    tracing::error!(
                        company_id = config.company_id,
                        %day,
                        attempt = claim.attempt,
                        %error,
                        "Company export failed"
                    );
                    run.failed += 1;
                    fail(state, claim.run_id, &error).await?;
                }
            }
        }
        run.alerts += send_alerts(state, &mailer, &config, day).await?;
    }
    Ok(run)
}

/// A day's run this call may do.
#[derive(Debug, Clone, Copy)]
struct Claim {
    run_id: i64,
    attempt: i32,
}

/// The run of `day` when this call gets it: the first attempt, a retry of a
/// failed one, or a run that died. `None` when it is sent, being done, or
/// out of attempts.
async fn claim(
    state: &AppState,
    company_id: i32,
    day: NaiveDate,
) -> Result<Option<Claim>, sqlx::Error> {
    let inserted = sqlx::query(
        "INSERT IGNORE INTO company_export_runs (company_id, export_date, status, attempts, started_at)
         VALUES (?, ?, 'running', 1, UTC_TIMESTAMP())",
    )
    .bind(company_id)
    .bind(day)
    .execute(&state.pool)
    .await?;
    if inserted.rows_affected() == 1 {
        return Ok(Some(Claim {
            run_id: i64::try_from(inserted.last_insert_id()).unwrap_or(i64::MAX),
            attempt: 1,
        }));
    }
    // A run that died counts as a failed attempt.
    sqlx::query(
        "UPDATE company_export_runs
            SET status = 'failed', finished_at = UTC_TIMESTAMP(),
                error = 'the export stopped before it finished'
          WHERE company_id = ? AND export_date = ? AND status = 'running'
            AND started_at < UTC_TIMESTAMP() - INTERVAL ? MINUTE",
    )
    .bind(company_id)
    .bind(day)
    .bind(STALE_AFTER_MINUTES)
    .execute(&state.pool)
    .await?;
    let retried = sqlx::query(
        "UPDATE company_export_runs
            SET status = 'running', attempts = attempts + 1, started_at = UTC_TIMESTAMP(),
                finished_at = NULL
          WHERE company_id = ? AND export_date = ? AND status = 'failed' AND attempts < ?
            AND finished_at < UTC_TIMESTAMP() - INTERVAL ? MINUTE",
    )
    .bind(company_id)
    .bind(day)
    .bind(MAX_ATTEMPTS)
    .bind(RETRY_AFTER_MINUTES)
    .execute(&state.pool)
    .await?;
    if retried.rows_affected() != 1 {
        return Ok(None);
    }
    let (run_id, attempt): (i64, i32) = sqlx::query_as(
        "SELECT id, CAST(attempts AS SIGNED) FROM company_export_runs WHERE company_id = ? AND export_date = ?",
    )
    .bind(company_id)
    .bind(day)
    .fetch_one(&state.pool)
    .await?;
    Ok(Some(Claim { run_id, attempt }))
}

/// Builds, stores and emails the export of `day`.
async fn export_and_send(
    state: &AppState,
    mailer: &Mailer,
    config: &Config,
    day: NaiveDate,
    local: DateTime<Tz>,
    claim: Claim,
) -> Result<(), String> {
    let company_id = config.company_id;
    let recipients = addresses(&config.recipients);
    if recipients.is_empty() {
        return Err("no recipients".into());
    }
    let bucket = state
        .signature_bucket
        .as_deref()
        .ok_or("no private bucket (SIGNATURE_STORAGE_BUCKET)")?
        .to_string();
    state
        .storage
        .verify_private(&bucket)
        .await
        .map_err(|error| format!("private bucket: {error}"))?;
    let db = |error: sqlx::Error| format!("database: {error}");
    let company_name: String = sqlx::query_scalar("SELECT name FROM company WHERE id = ?")
        .bind(company_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(db)?
        .ok_or("the company does not exist")?;
    let sent_runs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM company_export_runs WHERE company_id = ? AND status = 'sent'",
    )
    .bind(company_id)
    .fetch_one(&state.pool)
    .await
    .map_err(db)?;
    let full_files = sent_runs == 0 || local.weekday() == Weekday::Mon;
    let sent_before: HashSet<String> = if full_files {
        HashSet::new()
    } else {
        sqlx::query_scalar("SELECT file_hash FROM company_export_files WHERE company_id = ?")
            .bind(company_id)
            .fetch_all(&state.pool)
            .await
            .map_err(db)?
            .into_iter()
            .collect()
    };
    let built = archive::build(
        state,
        &archive::Request {
            company_id,
            company_name: &company_name,
            export_date: day,
            generated_at: local,
            full_files,
            sent_before: &sent_before,
        },
    )
    .await
    .map_err(|error| error.to_string())?;

    let key = format!("{PREFIX}/{company_id}/{day}-{}.zip", random_hex());
    let zip_bytes = built.zip.len();
    state
        .storage
        .put_private(&bucket, &key, built.zip, "application/zip")
        .await
        .map_err(|error| format!("upload: {error}"))?;
    sqlx::query("UPDATE company_export_runs SET object_key = ?, zip_bytes = ? WHERE id = ?")
        .bind(&key)
        .bind(i64::try_from(zip_bytes).unwrap_or(i64::MAX))
        .bind(claim.run_id)
        .execute(&state.pool)
        .await
        .map_err(db)?;

    let link_days = i64::from(config.keep_days).clamp(1, MAX_LINK_DAYS);
    let ttl = std::time::Duration::from_secs(u64::try_from(link_days * 86_400).unwrap_or(604_800));
    let file_name = format!("{} export {day}.zip", safe_file_name(&company_name));
    let link = state
        .storage
        .presign_get_in_for(Some(&bucket), &key, "application/zip", Some(&file_name), ttl)
        .await
        .map_err(|error| format!("link: {error}"))?;
    let expires = local + chrono::Duration::days(link_days);
    let (subject, html) = email(&company_name, day, &link, expires, &built.manifest);
    mailer
        .send(&recipients, &subject, &html)
        .await
        .map_err(|error| format!("email: {error}"))?;

    // Sent: from here on a failure must not send it again.
    let manifest = &built.manifest;
    let marked = sqlx::query(
        "UPDATE company_export_runs
            SET status = 'sent', finished_at = UTC_TIMESTAMP(), error = NULL,
                table_count = ?, row_count = ?, file_count = ?, full_files = ?,
                link_expires_at = UTC_TIMESTAMP() + INTERVAL ? DAY
          WHERE id = ?",
    )
    .bind(i64::try_from(manifest.tables.len()).unwrap_or(i64::MAX))
    .bind(i64::try_from(manifest.row_count).unwrap_or(i64::MAX))
    .bind(i64::try_from(manifest.files.included).unwrap_or(i64::MAX))
    .bind(full_files)
    .bind(link_days)
    .bind(claim.run_id)
    .execute(&state.pool)
    .await;
    if let Err(error) = marked {
        tracing::error!(%error, run_id = claim.run_id, "Company export sent but not marked sent");
    }
    if let Err(error) = record_files(state, company_id, claim.run_id, &built.included_files).await {
        tracing::error!(%error, run_id = claim.run_id, "Company export files not recorded");
    }
    tracing::info!(
        company_id,
        %day,
        tables = manifest.tables.len(),
        rows = manifest.row_count,
        files = manifest.files.included,
        zip_bytes,
        "Company export sent"
    );
    Ok(())
}

/// Remembers the files of this export, so the next one adds only new ones.
async fn record_files(
    state: &AppState,
    company_id: i32,
    run_id: i64,
    files: &[archive::IncludedFile],
) -> Result<(), sqlx::Error> {
    for chunk in files.chunks(200) {
        let mut builder = sqlx::QueryBuilder::<sqlx::MySql>::new(
            "INSERT IGNORE INTO company_export_files (company_id, file_hash, bucket, object_key, run_id, exported_at) ",
        );
        builder.push_values(chunk, |mut row, file| {
            row.push_bind(company_id)
                .push_bind(&file.hash)
                .push_bind(&file.bucket)
                .push_bind(&file.key)
                .push_bind(run_id)
                .push("UTC_TIMESTAMP()");
        });
        builder.build().execute(&state.pool).await?;
    }
    Ok(())
}

/// Marks the attempt failed; its zip, if uploaded, is deleted.
async fn fail(state: &AppState, run_id: i64, error: &str) -> Result<(), sqlx::Error> {
    let error: String = error.chars().take(2000).collect();
    sqlx::query(
        "UPDATE company_export_runs SET status = 'failed', finished_at = UTC_TIMESTAMP(), error = ?
          WHERE id = ?",
    )
    .bind(&error)
    .bind(run_id)
    .execute(&state.pool)
    .await?;
    let uploaded: Option<String> = sqlx::query_scalar(
        "SELECT object_key FROM company_export_runs WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(run_id)
    .fetch_optional(&state.pool)
    .await?
    .flatten();
    if let (Some(key), Some(bucket)) = (uploaded, state.signature_bucket.as_deref())
        && state.storage.delete_in(bucket, &key).await.is_ok()
    {
        sqlx::query("UPDATE company_export_runs SET deleted_at = UTC_TIMESTAMP(), object_key = NULL WHERE id = ?")
            .bind(run_id)
            .execute(&state.pool)
            .await?;
    }
    Ok(())
}

/// Emails `alert_recipients` about days that failed for good: out of
/// attempts, or an earlier day that never got sent. Each once.
async fn send_alerts(
    state: &AppState,
    mailer: &Mailer,
    config: &Config,
    today: NaiveDate,
) -> Result<usize, sqlx::Error> {
    let failed: Vec<(i64, NaiveDate, i32, Option<String>)> = sqlx::query_as(
        "SELECT id, export_date, CAST(attempts AS SIGNED), error FROM company_export_runs
          WHERE company_id = ? AND alerted_at IS NULL AND status = 'failed'
            AND (attempts >= ? OR export_date < ?)
          ORDER BY export_date",
    )
    .bind(config.company_id)
    .bind(MAX_ATTEMPTS)
    .bind(today)
    .fetch_all(&state.pool)
    .await?;
    if failed.is_empty() {
        return Ok(0);
    }
    let to = addresses(config.alert_recipients.as_deref().unwrap_or_default());
    let mut sent = 0;
    for (run_id, day, attempts, error) in failed {
        if to.is_empty() {
            tracing::error!(company_id = config.company_id, %day, "Company export failed; no alert recipients");
        } else {
            let subject = format!("Data export failed: company {} on {day}", config.company_id);
            let html = format!(
                "<p>The data export of company {company} for {day} failed after {attempts} \
                 attempts and was not sent.</p><p>Last error: <code>{error}</code></p>\
                 <p>Run id {run_id} in <code>company_export_runs</code>.</p>",
                company = config.company_id,
                error = crate::html::escape(error.as_deref().unwrap_or("unknown")),
            );
            if let Err(error) = mailer.send(&to, &subject, &html).await {
                tracing::error!(%error, run_id, "Company export alert not sent");
                continue;
            }
            sent += 1;
        }
        sqlx::query("UPDATE company_export_runs SET alerted_at = UTC_TIMESTAMP() WHERE id = ?")
            .bind(run_id)
            .execute(&state.pool)
            .await?;
    }
    Ok(sent)
}

/// Deletes zips older than their company's `keep_days` (7 without a row).
async fn delete_old_zips(state: &AppState) -> Result<usize, sqlx::Error> {
    let old: Vec<(i64, String)> = sqlx::query_as(
        "SELECT r.id, r.object_key FROM company_export_runs r
           LEFT JOIN company_exports c ON c.company_id = r.company_id
          WHERE r.object_key IS NOT NULL AND r.deleted_at IS NULL
            AND r.started_at < UTC_TIMESTAMP() - INTERVAL COALESCE(c.keep_days, 7) DAY
          LIMIT 50",
    )
    .fetch_all(&state.pool)
    .await?;
    let Some(bucket) = state.signature_bucket.as_deref() else {
        return Ok(0);
    };
    let mut deleted = 0;
    for (run_id, key) in old {
        match state.storage.delete_in(bucket, &key).await {
            Ok(()) => {
                sqlx::query("UPDATE company_export_runs SET deleted_at = UTC_TIMESTAMP() WHERE id = ?")
                    .bind(run_id)
                    .execute(&state.pool)
                    .await?;
                deleted += 1;
            }
            Err(error) => tracing::warn!(%error, run_id, "Old company export not deleted"),
        }
    }
    Ok(deleted)
}

fn random_hex() -> String {
    let mut bytes = [0u8; 12];
    getrandom::fill(&mut bytes).expect("system randomness");
    archive::hex(&bytes)
}

/// The company's name as a file name.
fn safe_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' { c } else { '_' })
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() { "Company".into() } else { cleaned }
}

/// The export email: subject and HTML body.
fn email(
    company_name: &str,
    day: NaiveDate,
    link: &str,
    expires: DateTime<Tz>,
    manifest: &archive::Manifest,
) -> (String, String) {
    let name = crate::html::escape(company_name);
    let date = day.format("%B %-d, %Y").to_string();
    let expires = expires.format("%B %-d, %Y %-I:%M %p %Z").to_string();
    let files = if manifest.full_files {
        format!("{} files (all pictures and documents)", manifest.files.included)
    } else {
        format!("{} new files since the last export", manifest.files.included)
    };
    let link = crate::html::escape(link);
    let subject = format!("{company_name} data export - {date}");
    let html = format!(
        "<div style=\"font-family:Arial,Helvetica,sans-serif;font-size:15px;color:#222\">\
         <p>The daily data export of <b>{name}</b> for {date} is ready.</p>\
         <p><a href=\"{link}\" style=\"display:inline-block;padding:10px 18px;background:#1f6feb;\
         color:#fff;text-decoration:none;border-radius:6px\">Download the export (zip)</a></p>\
         <p>{tables} tables, {rows} rows, {files}.</p>\
         <p>The link works until {expires}. The zip has one CSV file per table, the pictures \
         and documents, and a README.</p>\
         <p style=\"color:#666;font-size:13px\">Passwords, sign-in data and provider keys are \
         not included. If the button does not work, copy this link:<br>{link}</p></div>",
        tables = manifest.tables.len(),
        rows = manifest.row_count,
    );
    (subject, html)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_split_and_checked() {
        assert_eq!(
            addresses(" a@x.test, Name <b@y.test>;;nope , "),
            vec!["a@x.test".to_string(), "Name <b@y.test>".to_string()]
        );
        assert_eq!(addresses(""), Vec::<String>::new());
    }

    #[test]
    fn names_that_are_not_ascii_are_encoded_for_ses() {
        assert_eq!(ses_address(" a@x.test "), "a@x.test");
        assert_eq!(ses_address("Ann <a@x.test>"), "Ann <a@x.test>");
        assert_eq!(
            ses_address("Дмитрий Заика <d@x.test>"),
            "=?UTF-8?B?0JTQvNC40YLRgNC40Lkg0JfQsNC40LrQsA==?= <d@x.test>"
        );
    }

    #[test]
    fn file_names_are_safe() {
        assert_eq!(safe_file_name("GMQ Tops"), "GMQ Tops");
        assert_eq!(safe_file_name("A/B \"C\""), "A_B _C_");
        assert_eq!(safe_file_name("///"), "___");
        assert_eq!(safe_file_name("  "), "Company");
    }
}
