//! The API's MySQL pool, sized for Lambda.

use sqlx::Connection;
use sqlx::mysql::{MySqlConnectOptions, MySqlPool, MySqlPoolOptions};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A connection used this recently is handed to the next query without a
/// ping first.
const PING_AFTER_IDLE: Duration = Duration::from_secs(1);

/// The wall clock in milliseconds. Unlike the monotonic clock, it is known
/// to be right again when a frozen Lambda container wakes up.
fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// The pool every API Lambda container uses.
///
/// - Lazy: connecting on the first query instead of at startup means a full
///   database (`Too many connections`) fails that request with a JSON 500
///   that carries CORS headers. Connecting at startup made the container
///   exit, and API Gateway's bare 502 reached the browser as "Failed to fetch".
/// - One connection: a container serves one request at a time, and every
///   Lambda container counts against the database's limit (90), so the
///   connection count tracks the number of warm containers. No handler queries
///   the pool while holding a transaction or a streaming query, and the
///   `tokio::join!`s just run one after another.
/// - Session `wait_timeout` of 60 seconds: Lambda freezes, then discards,
///   containers without closing their connections, so the pool's own idle
///   timeout never runs. The server's setting (8 hours) let those idle
///   connections fill the database's limit; this makes MySQL drop them
///   itself. A warm container whose connection was dropped reconnects on its
///   next query.
/// - A ping before a query only after a pause: sqlx pings the connection
///   before every query and again when the query hands it back, two more
///   round trips to the database each time, and a handler runs ten to forty
///   queries one after another. The queries of one request follow each other
///   within milliseconds, so the ping before is kept only for a connection
///   idle for [`PING_AFTER_IDLE`] (by the pool's clock or the wall clock,
///   whichever says longer: a frozen container must never skip it), and one
///   that does not answer is replaced as before. The ping after stays: sqlx
///   always sends it.
pub fn lambda_pool(options: MySqlConnectOptions) -> MySqlPool {
    let released_at = Arc::new(AtomicU64::new(0));
    let stamp = Arc::clone(&released_at);
    MySqlPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .idle_timeout(Duration::from_secs(30))
        .max_lifetime(Duration::from_mins(10))
        .test_before_acquire(false)
        .before_acquire(move |conn, meta| {
            let since_release = Duration::from_millis(
                wall_ms().saturating_sub(released_at.load(Ordering::Relaxed)),
            );
            let fresh = meta.idle_for < PING_AFTER_IDLE && since_release < PING_AFTER_IDLE;
            Box::pin(async move {
                if fresh {
                    return Ok(true);
                }
                // Dropped by the database while the container slept: not an
                // error, the pool opens another.
                Ok(conn.ping().await.is_ok())
            })
        })
        .after_release(move |_conn, _meta| {
            stamp.store(wall_ms(), Ordering::Relaxed);
            Box::pin(async { Ok(true) })
        })
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                sqlx::query("SET SESSION wait_timeout = 60")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect_lazy_with(options)
}
