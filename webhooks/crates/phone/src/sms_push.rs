//! A new `RingCentral` or `CloudTalk` text on the phones of the people whose
//! inbox shows it. The API pushes it (`POST /v1/sms/inbound-push`): the
//! phones, FCM and who sees which conversation are there, not here.

use lambda_http::tracing;
use serde::Serialize;
use std::time::Duration;

/// The whole call may take this long. It is awaited, not spawned: the Lambda
/// freezes once it answers, so a spawned call might never run.
const PUSH_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Serialize)]
struct InboundTextPush {
    company_id: i32,
    provider: &'static str,
    sms_id: u64,
}

/// API root and the key to send.
#[cfg(not(test))]
fn api_endpoint() -> Option<(String, String)> {
    Some((
        std::env::var("API_URL").ok()?,
        std::env::var("LAMBDA_KEY").ok()?,
    ))
}

/// Tests get the loopback stub they installed (and no call at all
/// otherwise), never the real API.
#[cfg(test)]
fn api_endpoint() -> Option<(String, String)> {
    crate::tests::stub_http::api_base_url().map(|base| (base, "test-key".to_string()))
}

/// Asks the API to push the new inbound text `sms_id` (a row of
/// `provider`'s SMS table: `ringcentral` or `cloudtalk`) to the phones.
/// Waits [`PUSH_TIMEOUT`] at most; a failure is only logged, the text is
/// already stored.
pub async fn push_inbound_text(company_id: i32, provider: &'static str, sms_id: u64) {
    let Some((api_url, key)) = api_endpoint() else {
        return;
    };
    let body = InboundTextPush {
        company_id,
        provider,
        sms_id,
    };
    let result = common::http::client()
        .post(format!(
            "{}/v1/sms/inbound-push",
            api_url.trim_end_matches('/')
        ))
        .header("Authorization", key)
        .timeout(PUSH_TIMEOUT)
        .json(&body)
        .send()
        .await;
    match result {
        Ok(response) if response.status().is_success() => {}
        Ok(response) => {
            tracing::warn!(
                company_id,
                provider,
                status = response.status().as_u16(),
                "text push refused"
            );
        }
        Err(error) => {
            tracing::warn!(company_id, provider, error = %error.without_url(), "text push failed");
        }
    }
}
