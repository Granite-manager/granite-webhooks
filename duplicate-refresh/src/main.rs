//! Copies the production database (`main`) over the dev database
//! (`duplicate`), once a week (EventBridge Scheduler `duplicate-refresh`,
//! Sundays 2:00 America/New_York). Both live on the one RDS instance. See README.md.

use common::db::connect_options;
use lambda_runtime::{Error, LambdaEvent, run, service_fn, tracing};
use serde_json::{Value, json};
use sqlx::{Connection, MySqlConnection};

mod grants;
mod refresh;
mod reseal;

/// The secret (`SECRETS_ID`) holds it: the `duplicate_refresh` MySQL user,
/// which may only read `main`.
const URL_VAR: &str = "REFRESH_DATABASE_URL";

/// The API secrets whose keys encrypt two-step sign-in secrets: production
/// (reads `main`) and dev (reads `duplicate`). See `reseal.rs`.
const SOURCE_API_SECRET: &str = "granite/granite-api";
const TARGET_API_SECRET: &str = "granite/granite-api-dev";

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing::init_default_subscriber();
    common::amazon::secrets::load_into_env().await?;
    run(service_fn(handler)).await
}

/// `{"check_only": true}` checks the user's grants, the source and the
/// staging database without copying anything.
async fn handler(event: LambdaEvent<Value>) -> Result<Value, Error> {
    let check_only = event
        .payload
        .get("check_only")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let url = std::env::var(URL_VAR).map_err(|_| format!("{URL_VAR} is not set"))?;
    let mut keys = Vec::new();
    for name in [SOURCE_API_SECRET, TARGET_API_SECRET] {
        let pairs = common::amazon::secrets::get(name).await?;
        keys.push(reseal::Keys::from_secret(name, &pairs)?);
    }
    let target = keys.pop().ok_or("no dev keys")?;
    let source = keys.pop().ok_or("no production keys")?;
    let reseal = reseal::Reseal { source, target };
    let mut conn = MySqlConnection::connect_with(&connect_options(&url)?).await?;
    let result = refresh::run(
        &mut conn,
        &refresh::Schemas::PRODUCTION,
        &reseal,
        check_only,
    )
    .await;
    let _ = conn.close().await;
    match result {
        Ok(report) => {
            tracing::info!(?report, "duplicate refresh finished");
            Ok(json!({
                "check_only": check_only,
                "tables": report.tables,
                "rows": report.rows,
                "seconds": report.seconds,
            }))
        }
        Err(error) => {
            tracing::error!(%error, "duplicate refresh failed (before the swap, duplicate is left as it was)");
            Err(error.into())
        }
    }
}
