//! Builds a company's data export into a local zip without storing,
//! emailing or writing anything: every connection is read-only.
//!
//! ```sh
//! EXPORT_DATABASE_URL=mysql://... STORAGE_BUCKET=... STORAGE_ACCESS_KEY=... STORAGE_SECRET=... \
//!   SIGNATURE_STORAGE_BUCKET=... PHONE_RECORDINGS_BUCKET=... \
//!   cargo run -p api --example company_export_dry_run -- <company id> <out.zip>
//! ```
//!
//! `EXPORT_DATABASE_URL`, not `DATABASE_URL`: `.cargo/config.toml` pins that
//! one to the local MySQL.
//!
//! Prints the manifest's counts and checks each table's row count against
//! the database.

use api::routes::jobs::company_exports::{archive, tables};
use api::state::AppState;
use api::storage::Storage;
use sqlx::Executor as _;
use std::collections::HashSet;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let company_id: i32 = args.next().ok_or("company id")?.parse()?;
    let out = args.next().ok_or("output zip path")?;
    let url = std::env::var("EXPORT_DATABASE_URL")?;
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(1)
        .after_connect(|connection, _| {
            Box::pin(async move {
                connection
                    .execute("SET SESSION TRANSACTION READ ONLY")
                    .await
                    .map(|_| ())
            })
        })
        .connect_with(common::db::connect_options(&url)?)
        .await?;
    let mut state = AppState::new(pool.clone(), "dry-run", "")
        .with_signature_bucket(&std::env::var("SIGNATURE_STORAGE_BUCKET").unwrap_or_default())
        .with_storage(Storage::from_env());
    state.phone.recordings_bucket = std::env::var("PHONE_RECORDINGS_BUCKET")
        .ok()
        .map(std::sync::Arc::from);
    let name: String = sqlx::query_scalar("SELECT name FROM company WHERE id = ?")
        .bind(company_id)
        .fetch_optional(&pool)
        .await?
        .ok_or("no such company")?;
    let zone: chrono_tz::Tz = "America/New_York".parse()?;
    let now = chrono::Utc::now().with_timezone(&zone);
    let sent_before = HashSet::new();
    let built = archive::build(
        &state,
        &archive::Request {
            company_id,
            company_name: &name,
            export_date: now.date_naive(),
            generated_at: now,
            full_files: true,
            sent_before: &sent_before,
        },
    )
    .await
    .map_err(|error| format!("build: {error}"))?;
    std::fs::write(&out, &built.zip)?;
    let manifest = &built.manifest;
    let mut mismatches = 0;
    for entry in &manifest.tables {
        let (condition, binds) = tables::condition(&entry.table).ok_or("condition")?;
        let sql = format!("SELECT COUNT(*) FROM `{}` WHERE {condition}", entry.table);
        let mut query = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql));
        for _ in 0..binds {
            query = query.bind(company_id);
        }
        let count = usize::try_from(
            query
                .fetch_one(&pool)
                .await
                .map_err(|error| format!("{}: {error}", entry.table))?,
        )?;
        if count != entry.rows {
            mismatches += 1;
            println!(
                "MISMATCH {}: zip {} database {count}",
                entry.table, entry.rows
            );
        }
        if entry.rows > 0 {
            println!("{:>7}  {}", entry.rows, entry.table);
        }
    }
    println!(
        "{name}: {} tables, {} rows, files {:?}, unknown tables {:?}, missing tables {:?}, zip {} bytes, {mismatches} mismatches",
        manifest.tables.len(),
        manifest.row_count,
        manifest.files,
        manifest.unknown_tables,
        manifest.missing_tables,
        built.zip.len(),
    );
    Ok(())
}
