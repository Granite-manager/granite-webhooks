//! HTTP clients for the apps' dependency graph, which has no bundled TLS
//! crypto provider: rustls' default (aws-lc-rs) is a long C build, so the
//! clients use ring.

use std::sync::{Arc, Once, OnceLock};
use std::time::Duration;

/// Makes ring the process-wide rustls provider (once). reqwest is built with
/// `rustls-no-provider` and panics on a client built before this ran.
pub fn install_crypto_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Err: another provider was installed first, which works as well.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// How long an unused connection is kept for the next request. Short: a
/// Lambda is frozen between invocations, and a connection the server closed
/// meanwhile fails the next request instead of a fresh one being opened.
/// It is not enough on its own (see [`send_retrying`]).
const POOL_IDLE: Duration = Duration::from_secs(20);

/// `common::http::client_builder()` with the crypto provider installed.
#[allow(clippy::disallowed_methods)]
pub fn client_builder() -> reqwest::ClientBuilder {
    install_crypto_provider();
    reqwest::Client::builder()
        .pool_idle_timeout(POOL_IDLE)
        .tls_backend_preconfigured(tls_config())
}

/// The TLS settings every client shares (ring, TLS 1.2 and 1.3, h2 then
/// HTTP/1.1), built once.
///
/// The root certificates are Mozilla's, compiled in (`webpki-roots`, as sqlx
/// uses for RDS), not the system's: reading and parsing Amazon Linux's CA
/// bundle (rustls' platform verifier) took 60-75 ms of a 1024 MB Lambda's
/// cold start, before the first request to Secrets Manager could go out.
/// Amazon Linux's bundle is Mozilla's list as well. reqwest would also build
/// a verifier per client, and the api builds two dozen at cold start.
fn tls_config() -> rustls::ClientConfig {
    static CONFIG: OnceLock<rustls::ClientConfig> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let provider = rustls::crypto::CryptoProvider::get_default()
                .cloned()
                .unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()));
            let roots = rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            let mut config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("TLS 1.2 and 1.3")
                .with_root_certificates(roots)
                .with_no_client_auth();
            config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
            config
        })
        .clone()
}

/// `common::http::client()` with the crypto provider installed.
pub fn client() -> reqwest::Client {
    client_builder().build().expect("HTTP client")
}

/// Sends `request`, once more when the connection failed before any answer.
///
/// [`POOL_IDLE`] misses connections that went stale while the Lambda was
/// frozen (its clock stops then), so the next request on one fails at once
/// with "error sending request". A request that timed out is not sent again,
/// nor one whose body cannot be cloned. Only for requests that are safe to
/// repeat: a read, or a write that sets the same value again.
pub async fn send_retrying(
    request: reqwest::RequestBuilder,
) -> Result<reqwest::Response, reqwest::Error> {
    let again = request.try_clone();
    match (request.send().await, again) {
        (Err(error), Some(again)) if !error.is_timeout() => {
            let error = error_text(&error);
            tracing::warn!(%error, "request failed before any answer; sending it once more");
            again.send().await
        }
        (result, _) => result,
    }
}

/// reqwest's error with its causes, without the URL: "error sending request:
/// connection reset by peer", "timed out", "error decoding response body: ...".
pub fn error_text(error: &reqwest::Error) -> String {
    let mut text = if error.is_timeout() {
        "timed out"
    } else if error.is_decode() {
        "error decoding response body"
    } else if error.is_body() {
        "error reading response body"
    } else {
        "error sending request"
    }
    .to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.contains(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A server whose first `drops` connections close before any answer;
    /// the next one is answered `ok`.
    fn flaky_server(drops: usize) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}/q", listener.local_addr().expect("addr"));
        std::thread::spawn(move || {
            for (n, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { return };
                if n < drops {
                    continue;
                }
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                while !request.ends_with(b"\r\n\r\nx") {
                    match stream.read(&mut buffer) {
                        Ok(0) | Err(_) => return,
                        Ok(read) => request.extend_from_slice(&buffer[..read]),
                    }
                }
                let answer = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok";
                let _ = stream.write_all(answer);
            }
        });
        url
    }

    #[tokio::test]
    async fn a_connection_closed_before_any_answer_is_tried_once_more() {
        let url = flaky_server(1);
        let response = send_retrying(client().post(&url).body("x"))
            .await
            .expect("the second attempt is answered");
        assert_eq!(response.text().await.expect("body"), "ok");
    }

    #[tokio::test]
    async fn without_the_retry_that_request_fails() {
        let url = flaky_server(1);
        let error = client().post(&url).body("x").send().await.unwrap_err();
        assert!(!error.is_timeout());
        assert!(
            error_text(&error).starts_with("error sending request"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn it_is_tried_twice_at_most() {
        let url = flaky_server(2);
        assert!(send_retrying(client().post(&url).body("x")).await.is_err());
    }
}
