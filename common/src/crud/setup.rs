use sqlx::mysql::{MySqlPool, MySqlPoolOptions};
use std::env;
use std::time::Duration;

pub async fn create_db_pool() -> Result<MySqlPool, sqlx::Error> {
    // 1. Retrieve the URL from the environment
    let database_url =
        env::var("DATABASE_URL").expect("DATABASE_URL environment variable must be set");

    // 2. Establish and return the connection pool.
    //    sqlx defaults to 10 connections per pool; a warm Lambda container holds
    //    them open, so concurrent containers multiply straight into the RDS
    //    connection limit. A Lambda invocation is single-request, so one
    //    connection serves it (parallel queries just take turns), and the
    //    idle/lifetime caps release it once the container is thawed again.
    MySqlPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .idle_timeout(Duration::from_secs(60))
        .max_lifetime(Duration::from_secs(600))
        // Lambda freezes, then discards, containers without closing their
        // connection, so the idle timeout above never runs; the server's 600 s
        // `wait_timeout` kept those connections counting against the
        // database's limit (90). Two minutes is longer than any webhook or
        // job run (17 s and 88 s at most), so a live run is never cut off.
        // The global setting stays: timetree-sync holds one connection for
        // up to five minutes.
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                sqlx::query("SET SESSION wait_timeout = 120")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect_with(crate::db::connect_options(&database_url)?)
        .await
}
