//! The steps a sign-in may need after a right password.
//!
//! - `POST /v1/auth/login/code`                    → the authenticator app's code
//! - `POST /v1/auth/login/password`                → a new password (no sign-in asks for it now)
//! - `POST /v1/auth/login/authenticator`           → the QR code to add to the authenticator app
//! - `POST /v1/auth/login/authenticator/confirm`   → its first code: two-step sign-in is on
//!
//! `POST /v1/auth/login` (the CRM) and `POST /v1/auth/device-sessions` (the
//! Granite Manager app) answer 202 with a [`LoginChallenge`] instead of
//! signing in when the account needs one of these, in this order:
//!
//! 1. `code`: two-step sign-in is on; the code proves it is the user before
//!    anything else may change.
//! 2. `new_password`: kept for older challenges; no sign-in starts it now. A
//!    password of 1111 does not sign in at all ([`is_default_password`]):
//!    whoever typed it first could have chosen the new one, so the owner of
//!    the account sets it from a reset link instead.
//! 3. `setup_authenticator`: superusers must use two-step sign-in and have
//!    not set it up yet.
//!
//! Each step answers the next challenge (202, same token) or signs in as the
//! sign-in that started it would have: the CRM's 200 with the session cookie,
//! the app's 201 with its device token. A challenge works for 10 minutes;
//! wrong codes count with wrong passwords (`login_attempts`), and the fifth
//! wrong code of a challenge ends it. Sessions signed in before stay signed
//! in: the steps apply at sign-in only.

use crate::auth::sha256_hex;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::mfa::{self, AuthenticatorSetup};
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::routes::auth::{AuthSession, record_failed_login, start_session, too_many_failures};
use crate::routes::device_sessions::{self, DeviceSession, Platform};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::CACHE_CONTROL;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;
use std::fmt::Write;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(submit_code))
        .routes(routes!(set_password))
        .routes(routes!(start_authenticator))
        .routes(routes!(confirm_authenticator))
}

/// The old default password, which never signs in.
pub const DEFAULT_PASSWORD: &str = "1111";
/// Said to a right password of 1111.
pub const DEFAULT_PASSWORD_REFUSED: &str =
    "This password no longer works. Use \"Forgot password\" to set a new one.";

/// A sign-in whose (right) password is the old default 1111 is refused: the
/// account's owner sets a new one from a reset link, which proves the mailbox.
pub fn is_default_password(password: &str) -> bool {
    password == DEFAULT_PASSWORD
}
/// A challenge works this long.
const CHALLENGE_MINUTES: i64 = 10;
/// Wrong codes that end a challenge.
const MAX_WRONG_CODES: i32 = 5;
/// Challenge tokens: 32 random bytes, hex.
const TOKEN_BYTES: usize = 32;
/// Shortest new password.
const MIN_PASSWORD: usize = 8;
/// bcrypt cost of new passwords (as Remix).
const BCRYPT_COST: u32 = 10;

const EXPIRED: &str = "Your sign-in expired. Please sign in again.";
const WRONG_CODE: &str =
    "That code is not right. Check the code in your authenticator app and try again.";

/// What the sign-in needs next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LoginStep {
    /// Enter the 6-digit code of the authenticator app: `POST
    /// /v1/auth/login/code`.
    Code,
    /// The password is 1111: choose a new one (8 characters or more), `POST
    /// /v1/auth/login/password`.
    NewPassword,
    /// Superusers must use two-step sign-in: get the QR code (`POST
    /// /v1/auth/login/authenticator`), add it to an authenticator app and
    /// send its first code (`POST /v1/auth/login/authenticator/confirm`).
    SetupAuthenticator,
}

/// A sign-in that needs another step (HTTP 202).
#[derive(Debug, Serialize, ToSchema)]
pub struct LoginChallenge {
    /// The step to do now.
    pub step: LoginStep,
    /// Send it as `token` with the step. It works for 10 minutes and stays
    /// the same through every step of this sign-in.
    pub token: String,
}

/// The phone a sign-in of the app is for.
#[derive(Debug, Clone)]
pub struct PhoneDetails {
    pub device_name: String,
    pub platform: Platform,
    pub app_version: Option<String>,
}

#[derive(sqlx::FromRow)]
struct Challenge {
    id: i64,
    user_id: i32,
    email_hash: String,
    ip_hash: String,
    mfa_passed: bool,
    password_change: bool,
    failed_codes: i32,
    device_name: Option<String>,
    platform: Option<String>,
    app_version: Option<String>,
}

fn random_token() -> ApiResult<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|err| ApiError::Internal(err.to_string()))?;
    Ok(bytes
        .iter()
        .fold(String::with_capacity(TOKEN_BYTES * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        }))
}

/// The step a sign-in needs now, if any (one read).
async fn next_step(
    pool: &MySqlPool,
    user_id: i32,
    mfa_passed: bool,
    password_change: bool,
) -> sqlx::Result<Option<LoginStep>> {
    let mfa = mfa::state_of(pool, user_id).await?;
    Ok(if mfa.enabled && !mfa_passed {
        Some(LoginStep::Code)
    } else if password_change {
        Some(LoginStep::NewPassword)
    } else if !mfa.enabled && mfa.required {
        Some(LoginStep::SetupAuthenticator)
    } else {
        None
    })
}

/// The step a sign-in with a right password needs first, if any.
pub async fn first_step(pool: &MySqlPool, user_id: i32) -> sqlx::Result<Option<LoginStep>> {
    next_step(pool, user_id, false, false).await
}

/// A 202 with the challenge.
fn challenge_response(step: LoginStep, token: String) -> Response {
    (
        StatusCode::ACCEPTED,
        [(CACHE_CONTROL, "no-store")],
        Json(LoginChallenge { step, token }),
    )
        .into_response()
}

/// Starts the steps of a sign-in whose password was right: stores a
/// challenge and answers it (202).
pub async fn begin(
    pool: &MySqlPool,
    user_id: i32,
    step: LoginStep,
    email_hash: &str,
    ip_hash: &str,
    phone: Option<&PhoneDetails>,
) -> ApiResult<Response> {
    let token = random_token()?;
    sqlx::query("DELETE FROM login_challenges WHERE expires_at < NOW() LIMIT 500")
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO login_challenges
           (token_hash, user_id, email_hash, ip_hash, password_change,
            device_name, platform, app_version, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, NOW() + INTERVAL ? MINUTE)",
    )
    .bind(sha256_hex(&token))
    .bind(user_id)
    .bind(email_hash)
    .bind(ip_hash)
    .bind(false)
    .bind(phone.map(|p| p.device_name.as_str()))
    .bind(phone.map(|p| p.platform.as_str()))
    .bind(phone.and_then(|p| p.app_version.as_deref()))
    .bind(CHALLENGE_MINUTES)
    .execute(pool)
    .await?;
    Ok(challenge_response(step, token))
}

/// The live challenge of `token`.
async fn load(pool: &MySqlPool, token: &str) -> ApiResult<Challenge> {
    if token.len() != TOKEN_BYTES * 2 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ApiError::Unauthenticated(EXPIRED.into()));
    }
    sqlx::query_as(
        "SELECT CAST(c.id AS SIGNED) AS id, c.user_id, c.email_hash, c.ip_hash, c.mfa_passed,
                c.password_change, c.failed_codes, c.device_name, c.platform, c.app_version
           FROM login_challenges c
           JOIN users u ON u.id = c.user_id AND u.is_deleted = 0
          WHERE c.token_hash = ? AND c.expires_at > NOW()",
    )
    .bind(sha256_hex(token))
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| ApiError::Unauthenticated(EXPIRED.into()))
}

/// The challenge of `token` when `step` is what it needs now.
async fn load_at(pool: &MySqlPool, token: &str, step: LoginStep) -> ApiResult<Challenge> {
    let challenge = load(pool, token).await?;
    let now = next_step(
        pool,
        challenge.user_id,
        challenge.mfa_passed,
        challenge.password_change,
    )
    .await?;
    if now != Some(step) {
        return Err(ApiError::Conflict(
            "This sign-in step is already done. Please sign in again.".into(),
        ));
    }
    Ok(challenge)
}

/// Counts a wrong code: with the failed sign-ins, and on the challenge
/// (the fifth ends it).
async fn wrong_code(pool: &MySqlPool, challenge: &Challenge) -> ApiResult<Response> {
    record_failed_login(pool, &challenge.email_hash, &challenge.ip_hash).await?;
    if challenge.failed_codes + 1 >= MAX_WRONG_CODES {
        sqlx::query("DELETE FROM login_challenges WHERE id = ?")
            .bind(challenge.id)
            .execute(pool)
            .await?;
        return Err(ApiError::Unauthenticated(
            "Too many wrong codes. Please sign in again.".into(),
        ));
    }
    sqlx::query("UPDATE login_challenges SET failed_codes = failed_codes + 1 WHERE id = ?")
        .bind(challenge.id)
        .execute(pool)
        .await?;
    Err(ApiError::BadRequest(WRONG_CODE.into()))
}

/// Refuses while the sign-in's email or address has too many failures.
async fn check_limits(pool: &MySqlPool, challenge: &Challenge) -> ApiResult<()> {
    if too_many_failures(pool, &challenge.email_hash, &challenge.ip_hash).await? {
        return Err(ApiError::TooManyRequests(
            "Too many login attempts. Please try again later.".into(),
        ));
    }
    Ok(())
}

/// After a step: the next challenge, or the sign-in itself. `challenge`
/// carries the flags as the step left them.
async fn advance(state: &AppState, challenge: Challenge) -> ApiResult<Response> {
    let pool = &state.pool;
    if let Some(step) = next_step(
        pool,
        challenge.user_id,
        challenge.mfa_passed,
        challenge.password_change,
    )
    .await?
    {
        // The token is not stored; the client keeps the one it sent.
        return Ok((
            StatusCode::ACCEPTED,
            [(CACHE_CONTROL, "no-store")],
            Json(StepDone { step }),
        )
            .into_response());
    }
    // Spend the challenge first: a second request with it finds nothing.
    let spent = sqlx::query("DELETE FROM login_challenges WHERE id = ?")
        .bind(challenge.id)
        .execute(pool)
        .await?;
    if spent.rows_affected() == 0 {
        return Err(ApiError::Unauthenticated(EXPIRED.into()));
    }
    sqlx::query("DELETE FROM login_attempts WHERE email_hash = ?")
        .bind(&challenge.email_hash)
        .execute(pool)
        .await?;
    match challenge.platform.map(Platform::try_from) {
        None => start_session(state, challenge.user_id).await,
        Some(Ok(platform)) => {
            let phone = PhoneDetails {
                device_name: challenge.device_name.unwrap_or_default(),
                platform,
                app_version: challenge.app_version,
            };
            match device_sessions::phone_access_user(pool, challenge.user_id).await? {
                Some(user) => device_sessions::sign_phone_in(state, &user, &phone).await,
                None => Ok(device_sessions::no_phone_access()),
            }
        }
        Some(Err(_)) => Err(ApiError::Internal("unknown platform".into())),
    }
}

/// A step is done and another is needed (202); the token stays the same.
#[derive(Debug, Serialize, ToSchema)]
pub struct StepDone {
    /// The step to do now, with the same token.
    pub step: LoginStep,
}

/// Body of `POST /v1/auth/login/code` and
/// `POST /v1/auth/login/authenticator/confirm`.
#[derive(Deserialize, ToSchema)]
pub struct CodeInput {
    /// The sign-in's token (`LoginChallenge.token`).
    pub token: String,
    /// The 6-digit code the authenticator app shows now (spaces ignored).
    pub code: String,
}

impl std::fmt::Debug for CodeInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeInput").finish_non_exhaustive()
    }
}

/// Body of `POST /v1/auth/login/password`.
#[derive(Deserialize, ToSchema)]
pub struct PasswordInput {
    /// The sign-in's token (`LoginChallenge.token`).
    pub token: String,
    /// The new password: 8 characters or more (spaces around it are
    /// dropped, as sign-in drops them).
    pub password: String,
}

impl std::fmt::Debug for PasswordInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PasswordInput").finish_non_exhaustive()
    }
}

/// Body of `POST /v1/auth/login/authenticator`.
#[derive(Deserialize, ToSchema)]
pub struct TokenInput {
    /// The sign-in's token (`LoginChallenge.token`).
    pub token: String,
}

impl std::fmt::Debug for TokenInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenInput").finish_non_exhaustive()
    }
}

/// Send the authenticator code of a sign-in
///
/// The `code` step: checks the 6-digit code of the user's authenticator
/// app (the current one, or the one just before or after). A code works
/// once. Answers the next step (202 `StepDone`) or signs in: 200
/// `AuthSession` with the session cookie for a CRM sign-in, 201
/// `DeviceSession` for the app's. A wrong code is 400; the fifth wrong code
/// ends the sign-in (401), and wrong codes count with wrong passwords for
/// the 429 limits.
#[crate::openapi::path(
    post,
    path = "/v1/auth/login/code",
    operation_id = "submit_sign_in_code",
    tag = "auth",
    request_body = CodeInput,
    responses(
        (status = 200, description = "Signed in (CRM); the session cookie is set", body = AuthSession),
        (status = 201, description = "Signed in (app); the device token is in the body", body = DeviceSession),
        (status = 202, description = "Another step is needed", body = StepDone),
        (status = 400, description = "Wrong code", body = ErrorBody),
        (status = 401, description = "The sign-in expired or had too many wrong codes", body = ErrorBody),
        (status = 409, description = "This step is not the one the sign-in needs", body = ErrorBody),
        (status = 429, description = "Too many failed attempts", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn submit_code(
    State(state): State<AppState>,
    JsonBody(input): JsonBody<CodeInput>,
) -> ApiResult<Response> {
    let pool = &state.pool;
    let challenge = load_at(pool, &input.token, LoginStep::Code).await?;
    check_limits(pool, &challenge).await?;
    if !mfa::use_code(&state, challenge.user_id, &input.code).await? {
        return wrong_code(pool, &challenge).await;
    }
    sqlx::query("UPDATE login_challenges SET mfa_passed = TRUE WHERE id = ?")
        .bind(challenge.id)
        .execute(pool)
        .await?;
    advance(
        &state,
        Challenge {
            mfa_passed: true,
            ..challenge
        },
    )
    .await
}

/// Choose a new password at sign-in
///
/// The `new_password` step (the password was 1111): sets the new password
/// (8 characters or more, not 1111), then answers the next step (202
/// `StepDone`) or signs in: 200 `AuthSession` with the session cookie for a
/// CRM sign-in, 201 `DeviceSession` for the app's. Other sessions of the
/// user stay signed in.
#[crate::openapi::path(
    post,
    path = "/v1/auth/login/password",
    operation_id = "set_sign_in_password",
    tag = "auth",
    request_body = PasswordInput,
    responses(
        (status = 200, description = "Signed in (CRM); the session cookie is set", body = AuthSession),
        (status = 201, description = "Signed in (app); the device token is in the body", body = DeviceSession),
        (status = 202, description = "Another step is needed", body = StepDone),
        (status = 400, description = "Password too short, or 1111", body = ErrorBody),
        (status = 401, description = "The sign-in expired", body = ErrorBody),
        (status = 409, description = "This step is not the one the sign-in needs", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn set_password(
    State(state): State<AppState>,
    JsonBody(input): JsonBody<PasswordInput>,
) -> ApiResult<Response> {
    // Trimmed like sign-in does, so the new password always signs in.
    let password = input.password.trim().to_string();
    if password.chars().count() < MIN_PASSWORD || password.len() > 200 {
        return Err(ApiError::BadRequest(
            "Password must be at least 8 characters long.".into(),
        ));
    }
    let pool = &state.pool;
    let challenge = load_at(pool, &input.token, LoginStep::NewPassword).await?;
    let hash = tokio::task::spawn_blocking(move || bcrypt::hash(password, BCRYPT_COST))
        .await
        .map_err(|err| ApiError::Internal(err.to_string()))?
        .map_err(|err| ApiError::Internal(err.to_string()))?;
    let mut tx = pool.begin().await?;
    let changed = sqlx::query(
        "UPDATE login_challenges SET password_change = FALSE
          WHERE id = ? AND password_change = TRUE",
    )
    .bind(challenge.id)
    .execute(&mut *tx)
    .await?;
    if changed.rows_affected() == 0 {
        return Err(ApiError::Conflict(
            "This sign-in step is already done. Please sign in again.".into(),
        ));
    }
    sqlx::query("UPDATE users SET password = ? WHERE id = ? AND is_deleted = 0")
        .bind(hash)
        .bind(challenge.user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    advance(
        &state,
        Challenge {
            password_change: false,
            ..challenge
        },
    )
    .await
}

/// Get the authenticator QR code at sign-in
///
/// The `setup_authenticator` step (superusers must use two-step sign-in):
/// the secret to add to an authenticator app, as a QR code to scan, an
/// `otpauth://` link to open on the phone that has the app, and the secret
/// to type in. Asking again within 10 minutes answers the same secret.
/// Next: `POST /v1/auth/login/authenticator/confirm` with the code the app
/// then shows.
#[crate::openapi::path(
    post,
    path = "/v1/auth/login/authenticator",
    operation_id = "start_sign_in_authenticator",
    tag = "auth",
    request_body = TokenInput,
    responses(
        (status = 200, description = "What to add to the authenticator app", body = AuthenticatorSetup),
        (status = 401, description = "The sign-in expired", body = ErrorBody),
        (status = 409, description = "This step is not the one the sign-in needs", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn start_authenticator(
    State(state): State<AppState>,
    JsonBody(input): JsonBody<TokenInput>,
) -> ApiResult<Response> {
    let challenge = load_at(&state.pool, &input.token, LoginStep::SetupAuthenticator).await?;
    let email: String = sqlx::query_scalar("SELECT TRIM(email) FROM users WHERE id = ?")
        .bind(challenge.user_id)
        .fetch_one(&state.pool)
        .await?;
    let setup = mfa::start_setup(&state, challenge.user_id, &email).await?;
    Ok(([(CACHE_CONTROL, "no-store")], Json(setup)).into_response())
}

/// Turn on two-step sign-in at sign-in
///
/// The `setup_authenticator` step's end: the first code the authenticator
/// app shows for the secret of `POST /v1/auth/login/authenticator`. Turns
/// two-step sign-in on (every later sign-in asks for a code) and signs in:
/// 200 `AuthSession` with the session cookie for a CRM sign-in, 201
/// `DeviceSession` for the app's. A wrong code is 400; the fifth ends the
/// sign-in (401).
#[crate::openapi::path(
    post,
    path = "/v1/auth/login/authenticator/confirm",
    operation_id = "confirm_sign_in_authenticator",
    tag = "auth",
    request_body = CodeInput,
    responses(
        (status = 200, description = "Signed in (CRM); the session cookie is set", body = AuthSession),
        (status = 201, description = "Signed in (app); the device token is in the body", body = DeviceSession),
        (status = 202, description = "Another step is needed", body = StepDone),
        (status = 400, description = "Wrong code", body = ErrorBody),
        (status = 401, description = "The sign-in expired or had too many wrong codes", body = ErrorBody),
        (status = 409, description = "This step is not the one the sign-in needs", body = ErrorBody),
        (status = 429, description = "Too many failed attempts", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn confirm_authenticator(
    State(state): State<AppState>,
    JsonBody(input): JsonBody<CodeInput>,
) -> ApiResult<Response> {
    let pool = &state.pool;
    let challenge = load_at(pool, &input.token, LoginStep::SetupAuthenticator).await?;
    check_limits(pool, &challenge).await?;
    if !mfa::confirm_setup(&state, challenge.user_id, &input.code).await? {
        return wrong_code(pool, &challenge).await;
    }
    sqlx::query("UPDATE login_challenges SET mfa_passed = TRUE WHERE id = ?")
        .bind(challenge.id)
        .execute(pool)
        .await?;
    advance(
        &state,
        Challenge {
            mfa_passed: true,
            ..challenge
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_long_and_random() {
        let a = random_token().unwrap();
        assert_eq!(a.len(), 64);
        assert_ne!(a, random_token().unwrap());
    }
}
