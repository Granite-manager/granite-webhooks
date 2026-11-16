//! A user's own Gmail (My Account > Connect Gmail), for sending only.
//!
//! Granite Manager owns one Google OAuth app (`GOOGLE_OAUTH_CLIENT_ID` /
//! `GOOGLE_OAUTH_CLIENT_SECRET`). A user signs in with Google and allows
//! `gmail.send`; the refresh token is saved encrypted (`AES_KEY`) in
//! `gmail_user_logins`. Emails they send from the CRM then go out through
//! their Gmail (the webhooks Lambda's `Gmail` transport), so the company
//! needs no domain set up in Amazon SES.

use common::telephony::ringcentral_auth::{decrypt, encrypt, env_key};
use serde_json::Value;
use sqlx::MySqlPool;
use std::sync::Arc;
use std::time::Duration;

const AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";
/// Send only: Gmail can't be read with it.
pub const SEND_SCOPE: &str = "https://www.googleapis.com/auth/gmail.send";

/// The Google app users connect through.
#[derive(Clone)]
pub struct GmailApp {
    pub client_id: Arc<str>,
    pub client_secret: Arc<str>,
    /// Encrypts the saved refresh tokens (`AES_KEY`).
    pub aes_key: Option<[u8; 32]>,
}

#[derive(Debug, thiserror::Error)]
pub enum GmailError {
    #[error("Google refused the code ({0})")]
    Token(u16),
    #[error("Google request failed: {0}")]
    Request(String),
    #[error("Google's answer has no refresh token or address")]
    Incomplete,
    /// The user unticked "Send email on your behalf" on Google's page.
    #[error("the send permission was not given")]
    NoSendPermission,
    /// Google ended the connection (revoked, password changed, expired).
    #[error("Google ended the Gmail connection")]
    Revoked,
    #[error("AES_KEY is not set")]
    NoKey,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// A connected Gmail ready to send: its address and a fresh access token.
#[derive(Debug, Clone)]
pub struct SendingGmail {
    pub email: String,
    pub access_token: String,
}

fn encode(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
}

/// The address in a Google `id_token` (the JWT payload's `email`). The token
/// came straight from Google's token endpoint over TLS, so it isn't verified
/// again.
fn id_token_email(id_token: &str) -> Option<String> {
    use base64::Engine;
    let payload = id_token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims["email"]
        .as_str()
        .map(|email| email.trim().to_lowercase())
        .filter(|email| email.contains('@'))
}

impl GmailApp {
    /// `None` when the app's credentials are not set (connect unavailable).
    pub fn from_env() -> Option<Self> {
        let id = std::env::var("GOOGLE_OAUTH_CLIENT_ID").unwrap_or_default();
        let secret = std::env::var("GOOGLE_OAUTH_CLIENT_SECRET").unwrap_or_default();
        Self::new(&id, &secret, env_key())
    }

    pub fn new(id: &str, secret: &str, aes_key: Option<[u8; 32]>) -> Option<Self> {
        let (id, secret) = (id.trim(), secret.trim());
        (!id.is_empty() && !secret.is_empty()).then(|| Self {
            client_id: Arc::from(id),
            client_secret: Arc::from(secret),
            aes_key,
        })
    }

    /// Google's sign-in page, asking for the user's address and send-only
    /// Gmail access, with a refresh token (`access_type=offline`, and
    /// `prompt=consent` so a second connect gets one too).
    pub fn authorize_url(&self, redirect_uri: &str, state: &str) -> String {
        format!(
            "{AUTHORIZE_URL}?response_type=code&client_id={}&redirect_uri={}&state={}&scope={}&access_type=offline&prompt=consent&include_granted_scopes=true",
            encode(&self.client_id),
            encode(redirect_uri),
            encode(state),
            encode(&format!("openid email {SEND_SCOPE}")),
        )
    }

    /// Trades the code for tokens and saves the user's Gmail. Returns its
    /// address.
    pub async fn connect(
        &self,
        pool: &MySqlPool,
        client: &reqwest::Client,
        company_id: i32,
        user_id: i32,
        code: &str,
        redirect_uri: &str,
    ) -> Result<String, GmailError> {
        let key = self.aes_key.as_ref().ok_or(GmailError::NoKey)?;
        let tokens = self
            .token(
                client,
                &[
                    ("grant_type", "authorization_code"),
                    ("code", code),
                    ("redirect_uri", redirect_uri),
                ],
            )
            .await?;
        let granted = tokens["scope"].as_str().unwrap_or_default();
        if !granted.split_whitespace().any(|scope| scope == SEND_SCOPE) {
            return Err(GmailError::NoSendPermission);
        }
        let refresh = tokens["refresh_token"].as_str().unwrap_or_default();
        let email = tokens["id_token"].as_str().and_then(id_token_email);
        let (Some(email), false) = (email, refresh.is_empty()) else {
            return Err(GmailError::Incomplete);
        };
        sqlx::query(
            "INSERT INTO gmail_user_logins (user_id, company_id, email, refresh_token)
             VALUES (?, ?, ?, ?)
             ON DUPLICATE KEY UPDATE company_id = VALUES(company_id), email = VALUES(email),
                 refresh_token = VALUES(refresh_token), connected_at = NOW()",
        )
        .bind(user_id)
        .bind(company_id)
        .bind(&email)
        .bind(encrypt(key, refresh))
        .execute(pool)
        .await?;
        Ok(email)
    }

    /// The user's Gmail with a fresh access token; `None` when they haven't
    /// connected one. A connection Google ended is forgotten
    /// ([`GmailError::Revoked`]), so My Account shows Connect again.
    pub async fn sending(
        &self,
        pool: &MySqlPool,
        client: &reqwest::Client,
        user_id: i32,
    ) -> Result<Option<SendingGmail>, GmailError> {
        let row: Option<(String, Vec<u8>)> =
            sqlx::query_as("SELECT email, refresh_token FROM gmail_user_logins WHERE user_id = ?")
                .bind(user_id)
                .fetch_optional(pool)
                .await?;
        let Some((email, blob)) = row else {
            return Ok(None);
        };
        let key = self.aes_key.as_ref().ok_or(GmailError::NoKey)?;
        let Some(refresh) = decrypt(key, &blob) else {
            forget(pool, user_id).await?;
            return Err(GmailError::Revoked);
        };
        let tokens = match self
            .token(
                client,
                &[("grant_type", "refresh_token"), ("refresh_token", &refresh)],
            )
            .await
        {
            // `invalid_grant`: revoked, expired or the password changed.
            Err(GmailError::Token(400 | 401)) => {
                forget(pool, user_id).await?;
                return Err(GmailError::Revoked);
            }
            other => other?,
        };
        let access = tokens["access_token"].as_str().unwrap_or_default();
        if access.is_empty() {
            return Err(GmailError::Incomplete);
        }
        Ok(Some(SendingGmail {
            email,
            access_token: access.to_string(),
        }))
    }

    /// Ends the user's Gmail connection at Google (best effort) and forgets
    /// it. Whether there was one.
    pub async fn disconnect(
        &self,
        pool: &MySqlPool,
        client: &reqwest::Client,
        user_id: i32,
    ) -> Result<bool, sqlx::Error> {
        let blob: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT refresh_token FROM gmail_user_logins WHERE user_id = ?")
                .bind(user_id)
                .fetch_optional(pool)
                .await?;
        let Some(blob) = blob else {
            return Ok(false);
        };
        if let Some(refresh) = self.aes_key.as_ref().and_then(|key| decrypt(key, &blob)) {
            let body =
                serde_html_form::to_string([("token", refresh.as_str())]).unwrap_or_default();
            let revoked = client
                .post(REVOKE_URL)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(body)
                .timeout(Duration::from_secs(10))
                .send()
                .await;
            if let Err(error) = revoked {
                lambda_http::tracing::warn!(error = %error.without_url(), user_id, "Gmail revoke failed");
            }
        }
        forget(pool, user_id).await?;
        Ok(true)
    }

    async fn token(
        &self,
        client: &reqwest::Client,
        fields: &[(&str, &str)],
    ) -> Result<Value, GmailError> {
        let mut form: Vec<(&str, &str)> = vec![
            ("client_id", &self.client_id),
            ("client_secret", &self.client_secret),
        ];
        form.extend_from_slice(fields);
        let body = serde_html_form::to_string(&form)
            .map_err(|err| GmailError::Request(err.to_string()))?;
        let response = client
            .post(TOKEN_URL)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(body)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|err| GmailError::Request(err.without_url().to_string()))?;
        if !response.status().is_success() {
            return Err(GmailError::Token(response.status().as_u16()));
        }
        response
            .json()
            .await
            .map_err(|err| GmailError::Request(err.without_url().to_string()))
    }
}

/// The address of the user's connected Gmail, if any.
pub async fn connected_email(
    pool: &MySqlPool,
    user_id: i32,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT email FROM gmail_user_logins WHERE user_id = ?")
        .bind(user_id)
        .fetch_optional(pool)
        .await
}

async fn forget(pool: &MySqlPool, user_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM gmail_user_logins WHERE user_id = ?")
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_asks_for_send_only_with_a_refresh_token() {
        let app = GmailApp::new("id.apps", "secret", None).unwrap();
        let url = app.authorize_url(
            "https://granite-manager.com/redirects/google/gmail-auth",
            "s1",
        );
        assert!(url.starts_with(AUTHORIZE_URL), "{url}");
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("prompt=consent"));
        assert!(url.contains("state=s1"));
        assert!(url.contains(&encode(SEND_SCOPE)));
        assert!(!url.contains("gmail.readonly"));
        assert!(!url.contains("secret"));
    }

    #[test]
    fn app_needs_both_credentials() {
        assert!(GmailApp::new("", "secret", None).is_none());
        assert!(GmailApp::new("id", " ", None).is_none());
    }

    #[test]
    fn reads_the_address_from_the_id_token() {
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"email":"Vadim@GmqTops.com","email_verified":true}"#);
        assert_eq!(
            id_token_email(&format!("h.{payload}.sig")).as_deref(),
            Some("vadim@gmqtops.com")
        );
        assert_eq!(id_token_email("not-a-jwt"), None);
    }
}
