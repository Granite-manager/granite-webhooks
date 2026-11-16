//! Forgot password in the Granite Manager app: a code by email, no link.
//!
//! - `POST /v1/auth/password-reset-code`          → email a 6-digit code
//! - `POST /v1/auth/password-reset-code/confirm`  → the code and a new password
//!
//! The app has no web pages, so instead of the CRM's reset link the user
//! types the emailed code into the app with their new password. A code works
//! for 15 minutes and once; asking again ends the older one (and any reset or
//! invite link). It is kept in `password_reset_tokens` as SHA-256 of the user
//! id and the code, so it only matches that user. Wrong codes count with
//! wrong passwords (`login_attempts`, the same 429 limits as sign-in), and
//! the fifth failure since the code was sent ends it. Asking for a code says
//! the same whether or not the email exists. Setting the password ends every
//! session of the user, as the reset link does.

use crate::auth::sha256_hex;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::routes::auth::{
    BCRYPT_COST, MIN_PASSWORD, client_ip, record_failed_login, too_many_failures,
};
use crate::state::AppState;
use crate::webhooks_client::OutgoingEmail;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use lambda_http::tracing;
use serde::Deserialize;
use sqlx::MySqlPool;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(request_code))
        .routes(routes!(confirm_code))
}

/// A code works this long.
const CODE_MINUTES: i64 = 15;
/// One code email per user this often.
const CODE_INTERVAL_SECONDS: i64 = 60;
/// Failures since the code was sent that end it.
const MAX_WRONG_CODES: i64 = 5;
const WRONG_CODE: &str =
    "That code is wrong or has expired. Check the email or ask for a new code.";

/// Body of `POST /v1/auth/password-reset-code`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ResetCodeRequest {
    /// The account's email.
    pub email: String,
}

/// Body of `POST /v1/auth/password-reset-code/confirm`.
#[derive(Deserialize, ToSchema)]
pub struct ResetCodeConfirm {
    /// The account's email.
    pub email: String,
    /// The 6-digit code from the email.
    pub code: String,
    /// The new password, at least 8 characters.
    pub password: String,
}

impl std::fmt::Debug for ResetCodeConfirm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResetCodeConfirm").finish_non_exhaustive()
    }
}

/// The trimmed, lowercased email; `None` when it is not an address.
fn normalized_email(email: &str) -> Option<String> {
    let email = email.trim().to_lowercase();
    (email.len() <= 254
        && email
            .split_once('@')
            .is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.')))
    .then_some(email)
}

/// The account a reset for `email` is for (as the reset link picks it).
async fn reset_account(pool: &MySqlPool, email: &str) -> sqlx::Result<Option<(i32, String)>> {
    sqlx::query_as(
        "SELECT id, email FROM users
          WHERE LOWER(TRIM(email)) = ? AND is_deleted = 0 ORDER BY id LIMIT 1",
    )
    .bind(email)
    .fetch_optional(pool)
    .await
}

/// How a code is stored: only matches its own user.
fn code_hash(user_id: i32, code: &str) -> String {
    sha256_hex(&format!("reset-code:{user_id}:{code}"))
}

/// A random 6-digit code, every value equally likely.
fn random_code() -> Result<String, ApiError> {
    loop {
        let mut bytes = [0u8; 4];
        getrandom::fill(&mut bytes).map_err(|err| ApiError::Internal(err.to_string()))?;
        let value = u32::from_le_bytes(bytes);
        // 4_294_000_000 is a multiple of 1_000_000: no value is likelier.
        if value < 4_294_000_000 {
            return Ok(format!("{:06}", value % 1_000_000));
        }
    }
}

/// Email a password reset code
///
/// When the email belongs to an account (and no code or link was sent to it
/// in the last minute), stores a new 15-minute code (older codes and links
/// stop working) and emails it. Always answers 202 the same way, so it does
/// not tell which emails have accounts.
#[crate::openapi::path(
    post,
    path = "/v1/auth/password-reset-code",
    operation_id = "request_password_reset_code",
    tag = "auth",
    request_body = ResetCodeRequest,
    responses(
        (status = 202, description = "If the account exists, a code is on its way"),
        (status = 400, description = "Not an email address", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "The email could not be sent", body = ErrorBody),
    )
)]
pub async fn request_code(
    State(state): State<AppState>,
    JsonBody(input): JsonBody<ResetCodeRequest>,
) -> ApiResult<StatusCode> {
    let Some(email) = normalized_email(&input.email) else {
        return Err(ApiError::BadRequest(
            "Please enter a valid email address.".into(),
        ));
    };
    let pool = &state.pool;
    let Some((user_id, address)) = reset_account(pool, &email).await? else {
        return Ok(StatusCode::ACCEPTED);
    };
    let recent: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM password_reset_tokens
          WHERE user_id = ? AND created_at > NOW() - INTERVAL ? SECOND LIMIT 1",
    )
    .bind(user_id)
    .bind(CODE_INTERVAL_SECONDS)
    .fetch_optional(pool)
    .await?;
    if recent.is_some() {
        return Ok(StatusCode::ACCEPTED);
    }
    let code = random_code()?;
    let hash = code_hash(user_id, &code);
    sqlx::query("UPDATE password_reset_tokens SET used = TRUE WHERE user_id = ? AND used = FALSE")
        .bind(user_id)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO password_reset_tokens (user_id, token_hash, expires_at)
         VALUES (?, ?, NOW() + INTERVAL ? MINUTE)",
    )
    .bind(user_id)
    .bind(&hash)
    .bind(CODE_MINUTES)
    .execute(pool)
    .await?;
    let message = OutgoingEmail {
        from_name: Some("Granite Manager".into()),
        from_address: "noreply@granite-manager.com".into(),
        to: vec![address.trim().to_string()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: format!("{code} is your Granite Manager password reset code"),
        body: code_email_html(&code),
        signature: None,
        reply_to: Vec::new(),
        in_reply_to: None,
        references: Vec::new(),
        attachments: Vec::new(),
        transactional: true,
    };
    if let Err(err) = state.webhooks.send_email(&message).await {
        tracing::warn!(?err, user_id, "password reset code email failed");
        sqlx::query("UPDATE password_reset_tokens SET used = TRUE WHERE token_hash = ?")
            .bind(&hash)
            .execute(pool)
            .await?;
        return Err(ApiError::Upstream(
            "Failed to send email. Please try again later.".into(),
        ));
    }
    Ok(StatusCode::ACCEPTED)
}

fn code_email_html(code: &str) -> String {
    format!(
        r#"<div style="font-family: Arial, sans-serif; max-width: 480px; margin: 0 auto;">
  <h2 style="color: #1e293b;">Reset Your Password</h2>
  <p style="color: #475569; line-height: 1.6;">Enter this code in the Granite Manager app to choose a new password. It expires in {CODE_MINUTES} minutes.</p>
  <p style="font-size: 32px; font-weight: 700; letter-spacing: 8px; color: #0f172a; margin: 16px 0;">{code}</p>
  <p style="color: #94a3b8; font-size: 13px; margin-top: 24px;">If you didn't request this, you can safely ignore this email.</p>
</div>"#
    )
}

/// Set a new password with an emailed code
///
/// Uses a code once: sets the password (bcrypt), spends the code and ends
/// every session of the user. Then the app signs in with the new password.
#[crate::openapi::path(
    post,
    path = "/v1/auth/password-reset-code/confirm",
    operation_id = "confirm_password_reset_code",
    tag = "auth",
    request_body = ResetCodeConfirm,
    responses(
        (status = 204, description = "Password set; sign in with it"),
        (status = 400, description = "Wrong or expired code, or password too short", body = ErrorBody),
        (status = 429, description = "Too many failed attempts", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn confirm_code(
    State(state): State<AppState>,
    headers: HeaderMap,
    JsonBody(input): JsonBody<ResetCodeConfirm>,
) -> ApiResult<StatusCode> {
    // Trimmed like sign-in does, so the new password always signs in.
    let password = input.password.trim().to_string();
    if password.chars().count() < MIN_PASSWORD || password.len() > 200 {
        return Err(ApiError::BadRequest(
            "Password must be at least 8 characters long.".into(),
        ));
    }
    let code = input.code.trim();
    let Some(email) = normalized_email(&input.email) else {
        return Err(ApiError::BadRequest(WRONG_CODE.into()));
    };
    let pool = &state.pool;
    let email_hash = sha256_hex(&email);
    let ip_hash = sha256_hex(&client_ip(&headers));
    if too_many_failures(pool, &email_hash, &ip_hash).await? {
        return Err(ApiError::TooManyRequests(
            "Too many attempts. Please try again later.".into(),
        ));
    }
    let account = reset_account(pool, &email).await?;
    let well_formed = code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit());
    let Some((user_id, _)) = account.filter(|_| well_formed) else {
        record_failed_login(pool, &email_hash, &ip_hash).await?;
        return Err(ApiError::BadRequest(WRONG_CODE.into()));
    };
    let hash = code_hash(user_id, code);
    let live: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM password_reset_tokens
          WHERE user_id = ? AND token_hash = ? AND used = FALSE AND expires_at > NOW() LIMIT 1",
    )
    .bind(user_id)
    .bind(&hash)
    .fetch_optional(pool)
    .await?;
    if live.is_none() {
        record_failed_login(pool, &email_hash, &ip_hash).await?;
        end_after_too_many(pool, user_id, &email_hash).await?;
        return Err(ApiError::BadRequest(WRONG_CODE.into()));
    }
    let new_hash = tokio::task::spawn_blocking(move || bcrypt::hash(password, BCRYPT_COST))
        .await
        .map_err(|err| ApiError::Internal(err.to_string()))?
        .map_err(|err| ApiError::Internal(err.to_string()))?;
    let mut tx = pool.begin().await?;
    // Spend the code first: a second use of the same code finds nothing.
    let spent = sqlx::query(
        "UPDATE password_reset_tokens SET used = TRUE
          WHERE user_id = ? AND token_hash = ? AND used = FALSE AND expires_at > NOW()",
    )
    .bind(user_id)
    .bind(&hash)
    .execute(&mut *tx)
    .await?;
    if spent.rows_affected() == 0 {
        return Err(ApiError::BadRequest(WRONG_CODE.into()));
    }
    sqlx::query("UPDATE users SET password = ? WHERE id = ?")
        .bind(new_hash)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE sessions SET is_deleted = 1 WHERE user_id = ? AND is_deleted = 0")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM login_attempts WHERE email_hash = ?")
        .bind(&email_hash)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    crate::phone::revoke::revoke_user_phones(
        &state,
        user_id,
        crate::phone::revoke::RevokeReason::PasswordChanged,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Ends the user's live code once it has had `MAX_WRONG_CODES` failures
/// since it was sent.
async fn end_after_too_many(pool: &MySqlPool, user_id: i32, email_hash: &str) -> sqlx::Result<()> {
    let sent: Option<chrono::NaiveDateTime> = sqlx::query_scalar(
        "SELECT MAX(created_at) FROM password_reset_tokens
          WHERE user_id = ? AND used = FALSE AND expires_at > NOW()",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    let Some(sent) = sent else { return Ok(()) };
    let failures: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM login_attempts WHERE email_hash = ? AND attempted_at >= ?",
    )
    .bind(email_hash)
    .bind(sent)
    .fetch_one(pool)
    .await?;
    if failures >= MAX_WRONG_CODES {
        sqlx::query(
            "UPDATE password_reset_tokens SET used = TRUE WHERE user_id = ? AND used = FALSE",
        )
        .bind(user_id)
        .execute(pool)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_six_digits() {
        for _ in 0..50 {
            let code = random_code().unwrap();
            assert_eq!(code.len(), 6);
            assert!(code.bytes().all(|b| b.is_ascii_digit()));
        }
    }

    #[test]
    fn a_code_only_matches_its_user() {
        assert_ne!(code_hash(1, "123456"), code_hash(2, "123456"));
        assert_eq!(code_hash(1, "123456"), code_hash(1, "123456"));
    }

    #[test]
    fn emails_are_normalized() {
        assert_eq!(normalized_email("  A@B.com "), Some("a@b.com".into()));
        assert_eq!(normalized_email("nope"), None);
    }
}
