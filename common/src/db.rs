//! Connection options for the production MySQL (RDS).
//!
//! Every service reaches RDS over TLS, verified against Amazon's CA for
//! us-east-2 (`certs/rds-us-east-2-bundle.pem`, from
//! <https://truststore.pki.rds.amazonaws.com/us-east-2/us-east-2-bundle.pem>).
//!
//! Without TLS, MySQL 8's `caching_sha2_password` only lets a client in while
//! the server still holds the password in its cache. After a password change,
//! a reboot or a failover the cache is empty, the server asks for the full
//! exchange, and a client without TLS (and without RSA) is refused: "RSA auth
//! backend disabled". Over TLS the full exchange always works.

use sqlx::Connection;
use sqlx::mysql::{MySqlConnectOptions, MySqlPool, MySqlPoolOptions, MySqlSslMode};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Amazon RDS root certificates for us-east-2 (RSA2048, RSA4096, ECC384 G1).
const RDS_CA_BUNDLE: &[u8] = include_bytes!("../certs/rds-us-east-2-bundle.pem");

/// Parses a `mysql://` URL. An RDS host gets verified TLS: the certificate
/// must chain to Amazon's RDS CA and name the host, whatever `ssl-mode` the
/// URL names (the `granite/*` secrets say `DISABLED`). Other hosts (the local
/// test MySQL) keep the URL's own settings.
pub fn connect_options(url: &str) -> Result<MySqlConnectOptions, sqlx::Error> {
    let options = MySqlConnectOptions::from_str(url)?;
    Ok(if is_rds(options.get_host()) {
        options
            .ssl_mode(MySqlSslMode::VerifyIdentity)
            .ssl_ca_from_pem(RDS_CA_BUNDLE.to_vec())
    } else {
        options
    })
}

fn is_rds(host: &str) -> bool {
    host.to_ascii_lowercase().ends_with(".rds.amazonaws.com")
}

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

/// The pool every Lambda container of the API and of webhooks uses.
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
/// - Session `wait_timeout` of 120 seconds: Lambda freezes, then discards,
///   containers without closing their connections, so the pool's own idle
///   timeout never runs. The server's setting (8 hours) let those idle
///   connections fill the database's limit; this makes MySQL drop them
///   itself. A warm container whose connection was dropped reconnects on its
///   next query, which cost about 100 ms (TLS and sign-in on a 512 MB
///   Lambda) until sqlx's sockets got `TCP_NODELAY` (`vendor/sqlx-core`), and
///   about 10 ms since. 120, not 60: in a week of logs (Nov 2026) the most containers
///   that served a request within any 2 minutes was 64, the same as within
///   any minute (3 minutes: 75, against the limit of 90), and it cut the
///   requests that reconnect from 4.9% to 1.2%. The pool's idle timeout is
///   the same, so a connection is reused for as long as MySQL keeps it.
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
        .idle_timeout(Duration::from_secs(120))
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
                sqlx::query("SET SESSION wait_timeout = 120")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect_lazy_with(options)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rds_hosts_get_verified_tls() {
        let options = connect_options(
            "mysql://u:p@database-1-copy.c9uq4w6eak98.us-east-2.rds.amazonaws.com:3306/main",
        )
        .unwrap();
        assert!(matches!(
            options.get_ssl_mode(),
            MySqlSslMode::VerifyIdentity
        ));
    }

    #[test]
    fn other_hosts_keep_the_url_settings() {
        let options = connect_options("mysql://root:granite@127.0.0.1:3307/granite_local").unwrap();
        assert!(matches!(options.get_ssl_mode(), MySqlSslMode::Preferred));
    }

    #[test]
    fn the_bundle_holds_the_three_us_east_2_roots() {
        let pem = std::str::from_utf8(RDS_CA_BUNDLE).unwrap();
        assert_eq!(pem.matches("BEGIN CERTIFICATE").count(), 3);
    }
}
