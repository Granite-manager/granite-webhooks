//! Two-step sign-in with an authenticator app (TOTP, RFC 6238).
//!
//! A user's secret is 20 random bytes, shown once as base32 and as a QR code
//! of its `otpauth://` link (Google Authenticator, Microsoft Authenticator,
//! 1Password, Authy... read it). Codes are 6 digits, one per 30 seconds,
//! HMAC-SHA1, the standard every authenticator app uses. A code of the step
//! before or after also counts (phone clocks drift), and each step's code
//! works once (`user_mfa.last_step`).
//!
//! Secrets are stored encrypted with AES-256-GCM, keyed by `AES_KEY`; where
//! it is not set (local runs, tests) by the SHA-256 of `SESSION_SECRET`. The
//! stored value names its key (`a:` or `s:`), so setting `AES_KEY` later
//! does not lose the secrets saved before.

use crate::error::{ApiError, ApiResult};
use crate::openapi::ToSchema;
use crate::state::AppState;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::telephony::ringcentral_auth::{decrypt, encrypt, env_key};
use hmac::{Hmac, KeyInit, Mac};
use serde::Serialize;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use sqlx::MySqlPool;

/// The name authenticator apps show above the code.
pub const ISSUER: &str = "Granite Manager";
/// Secret length (160 bits, as RFC 4226 recommends).
const SECRET_BYTES: usize = 20;
/// Seconds per code.
const STEP_SECONDS: i64 = 30;
/// Steps before and after the current one whose codes still count.
const DRIFT_STEPS: i64 = 1;
/// Pixels per QR module.
const QR_SCALE: u32 = 8;
/// White modules around the QR code (the standard quiet zone).
const QR_QUIET: u32 = 4;
/// RFC 4648 base32 alphabet.
const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// What the user adds to their authenticator app.
#[derive(Debug, Serialize, ToSchema)]
pub struct AuthenticatorSetup {
    /// The secret as base32 (32 letters and digits), for typing into the
    /// authenticator app when the QR code cannot be scanned (setting up on
    /// the phone that shows it). Shown grouped by 4 is fine: apps ignore
    /// spaces.
    pub secret: String,
    /// The `otpauth://totp/...` link: opening it on a phone adds the account
    /// to the authenticator app installed there.
    pub uri: String,
    /// The QR code of `uri`, as a `data:image/png;base64,...` URL, for
    /// scanning with the authenticator app on another phone.
    pub qr: String,
}

/// RFC 4648 base32, no padding.
pub fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(BASE32[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// RFC 4648 base32 (any case, spaces and padding ignored); `None` when a
/// character is not base32.
pub fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 5 / 8);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for ch in text.chars().filter(|c| !c.is_whitespace() && *c != '=') {
        let upper = ch.to_ascii_uppercase();
        let value = BASE32.iter().position(|&b| char::from(b) == upper)?;
        buffer = (buffer << 5) | u32::try_from(value).ok()?;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xff).ok()?);
        }
    }
    Some(out)
}

/// A new random secret, base32.
pub fn new_secret() -> ApiResult<String> {
    let mut bytes = [0u8; SECRET_BYTES];
    getrandom::fill(&mut bytes).map_err(|err| ApiError::Internal(err.to_string()))?;
    Ok(base32_encode(&bytes))
}

/// The 6-digit code of `key` for the 30-second `step` (RFC 6238 / 4226).
pub fn code_at(key: &[u8], step: i64) -> u32 {
    let Ok(mut mac) = Hmac::<Sha1>::new_from_slice(key) else {
        return u32::MAX;
    };
    mac.update(&step.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = usize::from(digest[digest.len() - 1] & 0x0f);
    let binary = u32::from_be_bytes([
        digest[offset] & 0x7f,
        digest[offset + 1],
        digest[offset + 2],
        digest[offset + 3],
    ]);
    binary % 1_000_000
}

/// The code as typed (spaces and dashes ignored), when it is 6 digits.
pub fn normalized_code(input: &str) -> Option<u32> {
    let digits: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    (digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

/// The step whose code `code` is, among the current step and its
/// neighbors, newest first; `None` when it matches none of them.
pub fn matching_step(secret: &str, code: u32, unix_seconds: i64) -> Option<i64> {
    let key = base32_decode(secret)?;
    let now = unix_seconds.div_euclid(STEP_SECONDS);
    (-DRIFT_STEPS..=DRIFT_STEPS)
        .rev()
        .map(|delta| now + delta)
        .find(|&step| code_at(&key, step) == code)
}

/// The `otpauth://` link of the secret for the account `email`.
pub fn otpauth_uri(secret: &str, email: &str) -> String {
    use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
    let issuer = utf8_percent_encode(ISSUER, NON_ALPHANUMERIC);
    let account = utf8_percent_encode(email.trim(), NON_ALPHANUMERIC);
    format!(
        "otpauth://totp/{issuer}:{account}?secret={secret}&issuer={issuer}&algorithm=SHA1&digits=6&period={STEP_SECONDS}"
    )
}

/// A QR code of `text` as a PNG `data:` URL.
pub fn qr_data_url(text: &str) -> ApiResult<String> {
    use image::ImageEncoder;
    let code = qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::M)
        .map_err(|err| ApiError::Internal(format!("qr code: {err}")))?;
    let modules = u32::try_from(code.width()).map_err(|err| ApiError::Internal(err.to_string()))?;
    let colors = code.to_colors();
    let side = (modules + 2 * QR_QUIET) * QR_SCALE;
    let mut pixels = vec![255u8; (side * side) as usize];
    for (index, color) in colors.iter().enumerate() {
        if *color != qrcode::Color::Dark {
            continue;
        }
        let index = u32::try_from(index).map_err(|err| ApiError::Internal(err.to_string()))?;
        let (x, y) = (index % modules + QR_QUIET, index / modules + QR_QUIET);
        for dy in 0..QR_SCALE {
            let row = (y * QR_SCALE + dy) * side;
            let start = (row + x * QR_SCALE) as usize;
            pixels[start..start + QR_SCALE as usize].fill(0);
        }
    }
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&pixels, side, side, image::ExtendedColorType::L8)
        .map_err(|err| ApiError::Internal(format!("qr png: {err}")))?;
    Ok(format!("data:image/png;base64,{}", STANDARD.encode(png)))
}

/// What the user adds to their authenticator app for `secret`.
pub fn setup_for(secret: &str, email: &str) -> ApiResult<AuthenticatorSetup> {
    let uri = otpauth_uri(secret, email);
    Ok(AuthenticatorSetup {
        qr: qr_data_url(&uri)?,
        secret: secret.to_string(),
        uri,
    })
}

/// The key secrets are encrypted with, and its tag in the stored value.
fn sealing_key(state: &AppState) -> ([u8; 32], &'static str) {
    env_key().map_or_else(
        || (Sha256::digest(state.session_secret.as_bytes()).into(), "s:"),
        |key| (key, "a:"),
    )
}

/// The secret encrypted for storing.
pub fn seal(state: &AppState, secret: &str) -> String {
    let (key, tag) = sealing_key(state);
    format!("{tag}{}", encrypt(&key, secret))
}

/// A stored secret decrypted; `None` when its key is gone or it does not
/// open.
pub fn open(state: &AppState, stored: &str) -> Option<String> {
    let (tag, sealed) = stored.split_at_checked(2)?;
    match tag {
        "a:" => decrypt(&env_key()?, sealed.as_bytes()),
        // Sealed under the secret of its day: during a rotation, either one.
        "s:" => state.session_secrets.all().find_map(|secret| {
            let key: [u8; 32] = Sha256::digest(secret.as_bytes()).into();
            decrypt(&key, sealed.as_bytes())
        }),
        _ => None,
    }
}

/// Seconds since 1970, as codes count time.
pub fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// A user's two-step sign-in, in one read.
#[derive(Debug, Clone, Copy, sqlx::FromRow)]
pub struct MfaState {
    /// It is on.
    pub enabled: bool,
    /// The user must use it (superusers).
    pub required: bool,
}

/// The user's [`MfaState`] (both false for an unknown user).
pub async fn state_of(pool: &MySqlPool, user_id: i32) -> sqlx::Result<MfaState> {
    Ok(sqlx::query_as(
        "SELECT (m.enabled_at IS NOT NULL AND m.secret IS NOT NULL) AS enabled,
                (COALESCE(u.is_superuser, 0) <> 0) AS required
           FROM users u LEFT JOIN user_mfa m ON m.user_id = u.id
          WHERE u.id = ? AND u.is_deleted = 0",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?
    .unwrap_or(MfaState {
        enabled: false,
        required: false,
    }))
}

/// Uses `code` if it is the user's current code: true when it is (and it
/// can never be used again), false when it is wrong or already used.
pub async fn use_code(state: &AppState, user_id: i32, code: &str) -> ApiResult<bool> {
    let Some(code) = normalized_code(code) else {
        return Ok(false);
    };
    let stored: Option<Option<String>> =
        sqlx::query_scalar("SELECT secret FROM user_mfa WHERE user_id = ? AND enabled_at IS NOT NULL")
            .bind(user_id)
            .fetch_optional(&state.pool)
            .await?;
    let Some(secret) = stored.flatten().and_then(|stored| open(state, &stored)) else {
        return Ok(false);
    };
    let Some(step) = matching_step(&secret, code, unix_now()) else {
        return Ok(false);
    };
    // Spends the step: a second use of the same code (or an older one) finds
    // nothing to update.
    let spent = sqlx::query(
        "UPDATE user_mfa SET last_step = ?
          WHERE user_id = ? AND (last_step IS NULL OR last_step < ?)",
    )
    .bind(step)
    .bind(user_id)
    .bind(step)
    .execute(&state.pool)
    .await?;
    Ok(spent.rows_affected() == 1)
}

/// Starts setting up an authenticator: what to add to the app.
///
/// A new pending secret, or the unconfirmed one of the last 10 minutes, so
/// the QR code on screen keeps working while the user scans it.
pub async fn start_setup(
    state: &AppState,
    user_id: i32,
    email: &str,
) -> ApiResult<AuthenticatorSetup> {
    let pending: Option<Option<String>> = sqlx::query_scalar(
        "SELECT pending_secret FROM user_mfa
          WHERE user_id = ? AND pending_created_at > NOW() - INTERVAL 10 MINUTE",
    )
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await?;
    if let Some(secret) = pending.flatten().and_then(|stored| open(state, &stored)) {
        return setup_for(&secret, email);
    }
    let secret = new_secret()?;
    sqlx::query(
        "INSERT INTO user_mfa (user_id, pending_secret, pending_created_at) VALUES (?, ?, NOW())
         ON DUPLICATE KEY UPDATE pending_secret = VALUES(pending_secret),
                                 pending_created_at = VALUES(pending_created_at)",
    )
    .bind(user_id)
    .bind(seal(state, &secret))
    .execute(&state.pool)
    .await?;
    setup_for(&secret, email)
}

/// Turns two-step sign-in on with the pending secret when `code` is its
/// current code: true when it did. The code is spent.
pub async fn confirm_setup(state: &AppState, user_id: i32, code: &str) -> ApiResult<bool> {
    let Some(code) = normalized_code(code) else {
        return Ok(false);
    };
    let pending: Option<Option<String>> = sqlx::query_scalar(
        "SELECT pending_secret FROM user_mfa
          WHERE user_id = ? AND pending_created_at > NOW() - INTERVAL 1 HOUR",
    )
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await?;
    let Some(stored) = pending.flatten() else {
        return Ok(false);
    };
    let Some(secret) = open(state, &stored) else {
        return Ok(false);
    };
    let Some(step) = matching_step(&secret, code, unix_now()) else {
        return Ok(false);
    };
    let saved = sqlx::query(
        "UPDATE user_mfa
            SET secret = pending_secret, enabled_at = NOW(), last_step = ?,
                pending_secret = NULL, pending_created_at = NULL
          WHERE user_id = ? AND pending_secret = ?",
    )
    .bind(step)
    .bind(user_id)
    .bind(&stored)
    .execute(&state.pool)
    .await?;
    Ok(saved.rows_affected() == 1)
}

/// Turns two-step sign-in off for the user.
pub async fn disable(pool: &MySqlPool, user_id: i32) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM user_mfa WHERE user_id = ?")
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_round_trips() {
        let bytes: Vec<u8> = (0u8..=40).collect();
        let text = base32_encode(&bytes);
        assert_eq!(base32_decode(&text).unwrap(), bytes);
        assert_eq!(base32_encode(b"foobar"), "MZXW6YTBOI");
        assert_eq!(base32_decode("mzxw 6ytb oi==").unwrap(), b"foobar");
        assert!(base32_decode("MZ1W").is_none());
    }

    /// RFC 6238 appendix B, SHA-1, last 6 digits.
    #[test]
    fn codes_match_the_rfc() {
        let key = b"12345678901234567890";
        for (time, code) in [
            (59, 287_082),
            (1_111_111_109, 81_804),
            (1_111_111_111, 50_471),
            (1_234_567_890, 5_924),
            (2_000_000_000, 279_037),
        ] {
            assert_eq!(code_at(key, time / STEP_SECONDS), code, "{time}");
        }
    }

    #[test]
    fn neighbors_count_and_others_do_not() {
        let secret = base32_encode(b"12345678901234567890");
        let key = base32_decode(&secret).unwrap();
        let now = 1_111_111_111;
        let step = now / STEP_SECONDS;
        assert_eq!(matching_step(&secret, code_at(&key, step), now), Some(step));
        assert_eq!(
            matching_step(&secret, code_at(&key, step - 1), now),
            Some(step - 1)
        );
        assert_eq!(
            matching_step(&secret, code_at(&key, step + 1), now),
            Some(step + 1)
        );
        assert_eq!(matching_step(&secret, code_at(&key, step - 3), now), None);
    }

    #[test]
    fn codes_are_six_digits() {
        assert_eq!(normalized_code(" 123 456 "), Some(123_456));
        assert_eq!(normalized_code("012-345"), Some(12_345));
        assert_eq!(normalized_code("12345"), None);
        assert_eq!(normalized_code("12345a"), None);
    }

    #[test]
    fn the_link_and_qr_code() {
        let uri = otpauth_uri("ABCD", "rep@acme.test");
        assert_eq!(
            uri,
            "otpauth://totp/Granite%20Manager:rep%40acme%2Etest?secret=ABCD&issuer=Granite%20Manager&algorithm=SHA1&digits=6&period=30"
        );
        let qr = qr_data_url(&uri).unwrap();
        assert!(qr.starts_with("data:image/png;base64,iVBOR"));
    }
}
