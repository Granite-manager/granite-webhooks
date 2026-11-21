//! Uploads a company's export zip to the company's own Google Cloud Storage
//! bucket (`company_exports.gcs_bucket`), as the service account in
//! `GCS_SERVICE_ACCOUNT_JSON`. The company grants that account
//! *Storage Object Creator* on its bucket: it can add files, never read,
//! overwrite or delete them, so every upload has a new name.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const SCOPE: &str = "https://www.googleapis.com/auth/devstorage.read_write";
const UPLOAD_URL: &str = "https://storage.googleapis.com/upload/storage/v1/b";

/// Where the zips go: Google Cloud Storage, or a list for tests.
#[derive(Clone, Default)]
pub enum Uploader {
    #[default]
    Google,
    Memory(Arc<Mutex<Vec<Uploaded>>>),
}

/// A zip the [`Uploader::Memory`] took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uploaded {
    pub bucket: String,
    pub name: String,
    pub bytes: Vec<u8>,
}

impl Uploader {
    /// Stores `bytes` as `name` in `bucket`; returns its `gs://` address.
    /// An object already there is never replaced (the upload fails).
    pub async fn upload(&self, bucket: &str, name: &str, bytes: Vec<u8>) -> Result<String, String> {
        match self {
            Self::Google => {
                let json = std::env::var("GCS_SERVICE_ACCOUNT_JSON")
                    .ok()
                    .filter(|json| !json.trim().is_empty())
                    .ok_or("no Google service account (GCS_SERVICE_ACCOUNT_JSON)")?;
                let account = ServiceAccount::parse(&json)?;
                upload_to_google(&account, bucket, name, bytes).await?;
            }
            Self::Memory(uploaded) => uploaded.lock().expect("lock").push(Uploaded {
                bucket: bucket.to_string(),
                name: name.to_string(),
                bytes,
            }),
        }
        Ok(format!("gs://{bucket}/{name}"))
    }
}

async fn upload_to_google(
    account: &ServiceAccount,
    bucket: &str,
    name: &str,
    bytes: Vec<u8>,
) -> Result<(), String> {
    let http = common::http::client_builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|error| format!("HTTP client: {error}"))?;
    let assertion = account.jwt(chrono::Utc::now().timestamp())?;
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer")
        .append_pair("assertion", &assertion)
        .finish();
    let response = http
        .post(&account.token_uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|error| format!("Google sign-in: {}", error.without_url()))?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        return Err(format!(
            "Google sign-in answered {status}: {}",
            text.chars().take(300).collect::<String>()
        ));
    }
    let answer: Value = response
        .json()
        .await
        .map_err(|error| format!("Google sign-in: {}", error.without_url()))?;
    let token = answer["access_token"]
        .as_str()
        .filter(|token| !token.is_empty())
        .ok_or("Google sign-in gave no access token")?;

    let mut url = url::Url::parse(&format!("{UPLOAD_URL}/{bucket}/o"))
        .map_err(|error| format!("bucket name: {error}"))?;
    url.query_pairs_mut()
        .append_pair("uploadType", "media")
        .append_pair("name", name)
        .append_pair("ifGenerationMatch", "0");
    let response = http
        .post(url)
        .bearer_auth(token)
        .header("content-type", "application/zip")
        .body(bytes)
        .send()
        .await
        .map_err(|error| format!("Google upload: {}", error.without_url()))?;
    if response.status().is_success() {
        return Ok(());
    }
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let why = match status {
        403 => " (the service account needs Storage Object Creator on the bucket)",
        404 => " (no such bucket)",
        412 => " (a file with this name is already there)",
        _ => "",
    };
    Err(format!(
        "Google upload answered {status}{why}: {}",
        text.chars().take(300).collect::<String>()
    ))
}

struct ServiceAccount {
    client_email: String,
    token_uri: String,
    key: RsaKeyPair,
}

impl ServiceAccount {
    /// A Google service-account JSON key (`client_email`, `private_key` as a
    /// PKCS#8 PEM, optional `token_uri`).
    fn parse(json: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(json)
            .map_err(|_| "the service account is not JSON".to_string())?;
        let field = |name: &str| {
            value[name]
                .as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
                .ok_or_else(|| format!("the service account has no {name}"))
        };
        let pem = field("private_key")?;
        let der: String = pem
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with("-----"))
            .collect();
        let der = STANDARD
            .decode(der)
            .map_err(|_| "the private key is not PEM".to_string())?;
        let key = RsaKeyPair::from_pkcs8(&der)
            .map_err(|error| format!("the private key is rejected: {error}"))?;
        Ok(Self {
            client_email: field("client_email")?,
            token_uri: field("token_uri").unwrap_or_else(|_| GOOGLE_TOKEN_URL.to_string()),
            key,
        })
    }

    /// The signed JWT the token endpoint swaps for an access token.
    fn jwt(&self, now: i64) -> Result<String, String> {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = json!({
            "iss": self.client_email,
            "scope": SCOPE,
            "aud": self.token_uri,
            "iat": now,
            "exp": now + 3600,
        });
        let claims = URL_SAFE_NO_PAD.encode(claims.to_string());
        let signing_input = format!("{header}.{claims}");
        let mut signature = vec![0; self.key.public().modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                signing_input.as_bytes(),
                &mut signature,
            )
            .map_err(|_| "signing the Google sign-in failed".to_string())?;
        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature)
        ))
    }
}

/// The object name of a day's export: `<company>/<day>.zip`, with the
/// attempt number after a failed attempt (the bucket never replaces a file).
pub fn object_name(company_folder: &str, day: chrono::NaiveDate, attempt: i32) -> String {
    if attempt <= 1 {
        format!("{company_folder}/{day}.zip")
    } else {
        format!("{company_folder}/{day}-attempt-{attempt}.zip")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_get_the_attempt_after_a_failure() {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 11, 21).unwrap();
        assert_eq!(
            object_name("GMQ TOPS Mobile", day, 1),
            "GMQ TOPS Mobile/2026-11-21.zip"
        );
        assert_eq!(
            object_name("GMQ TOPS Mobile", day, 2),
            "GMQ TOPS Mobile/2026-11-21-attempt-2.zip"
        );
    }

    #[test]
    fn a_broken_service_account_is_refused() {
        assert!(ServiceAccount::parse("nope").is_err());
        assert!(ServiceAccount::parse(r#"{"client_email":"a@b"}"#).is_err());
    }
}
