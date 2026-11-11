//! The walk-in check-in tablet's sign-in: no user and no password.
//!
//! An admin signs in on the tablet and makes it a walk-in tablet
//! (`POST /v1/check-in/tablets/this-device`): the browser gets a key, kept
//! in the [`CHECK_IN_COOKIE`] cookie, and the admin is signed out. The key
//! opens only that company's walk-in
//! form (`/v1/check-in/kiosk...`) and the address lookup the form uses, until
//! an admin removes the tablet. It never ends by time.
//!
//! Keys are [`super::device::new_token`]s (43 characters), stored only as
//! their SHA-256 (hex).

use super::cookie::cookie_values;
use super::device::{TOKEN_LEN, token_hash};
use super::session::SessionUser;
use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::FromRequestParts;
use axum::http::HeaderMap;
use axum::http::header::COOKIE;
use axum::http::request::Parts;
use sqlx::{FromRow, MySqlPool};

/// The cookie a paired tablet keeps its key in.
pub const CHECK_IN_COOKIE: &str = "granite_check_in";
/// `last_seen_at` is written at most this often.
const SEEN_EVERY_SECONDS: i64 = 300;

/// A paired walk-in tablet.
#[derive(Debug, Clone, FromRow)]
pub struct CheckInTablet {
    /// `check_in_devices.id`.
    pub id: i32,
    /// The company whose walk-in form it shows.
    pub company_id: i32,
    /// Its name (shown on Company, and as who created its walk-ins).
    pub name: String,
}

#[derive(FromRow)]
struct LiveTablet {
    id: i32,
    company_id: i32,
    name: String,
    /// `last_seen_at` is due for a write.
    stale: bool,
}

/// The shape of a key we issue; anything else is refused unread.
fn well_formed(key: &str) -> bool {
    key.len() == TOKEN_LEN
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The tablet key in a request's cookies, when it has one.
pub fn request_key(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .flat_map(|header| cookie_values(header, CHECK_IN_COOKIE))
        .find(|key| well_formed(key))
}

/// The live paired tablet of `key`: not removed, and its company not deleted.
pub async fn find_tablet(
    pool: &MySqlPool,
    key: &str,
) -> Result<Option<CheckInTablet>, sqlx::Error> {
    if !well_formed(key) {
        return Ok(None);
    }
    let tablet: Option<LiveTablet> = sqlx::query_as(
        "SELECT d.id, d.company_id, d.name,
                (d.last_seen_at IS NULL
                 OR d.last_seen_at < UTC_TIMESTAMP() - INTERVAL ? SECOND) AS stale
           FROM check_in_devices d
           JOIN company co ON co.id = d.company_id AND co.deleted_at IS NULL
          WHERE d.token_hash = ? AND d.paired_at IS NOT NULL AND d.revoked_at IS NULL",
    )
    .bind(SEEN_EVERY_SECONDS)
    .bind(token_hash(key))
    .fetch_optional(pool)
    .await?;
    let Some(tablet) = tablet else {
        return Ok(None);
    };
    if tablet.stale {
        sqlx::query("UPDATE check_in_devices SET last_seen_at = UTC_TIMESTAMP() WHERE id = ?")
            .bind(tablet.id)
            .execute(pool)
            .await?;
    }
    Ok(Some(CheckInTablet {
        id: tablet.id,
        company_id: tablet.company_id,
        name: tablet.name,
    }))
}

/// The request's paired tablet, else 401 (the tablet shows the pairing
/// screen again).
impl FromRequestParts<AppState> for CheckInTablet {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let Some(key) = request_key(&parts.headers) else {
            return Err(ApiError::Unauthorized);
        };
        find_tablet(&state.pool, &key)
            .await?
            .ok_or(ApiError::Unauthorized)
    }
}

/// Someone signed in (as [`super::CurrentUser`] takes them), or else a
/// paired walk-in tablet. For what the walk-in form shares with the CRM (the
/// address lookup).
pub enum UserOrTablet {
    /// A signed-in user.
    User(SessionUser),
    /// A paired walk-in tablet.
    Tablet(CheckInTablet),
}

impl FromRequestParts<AppState> for UserOrTablet {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        match super::authenticate(parts, state).await {
            Ok(user) => Ok(Self::User(user)),
            Err(ApiError::Unauthorized) => CheckInTablet::from_request_parts(parts, state)
                .await
                .map(Self::Tablet),
            Err(other) => Err(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_issued_keys_are_read() {
        let key = "a".repeat(TOKEN_LEN);
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            format!("__session=x; {CHECK_IN_COOKIE}={key}")
                .parse()
                .unwrap(),
        );
        assert_eq!(request_key(&headers), Some(key));
        let mut short = HeaderMap::new();
        short.insert(COOKIE, format!("{CHECK_IN_COOKIE}=abc").parse().unwrap());
        assert_eq!(request_key(&short), None);
        let mut odd = HeaderMap::new();
        let bad = format!("{}%", "a".repeat(TOKEN_LEN - 1));
        odd.insert(COOKIE, format!("{CHECK_IN_COOKIE}={bad}").parse().unwrap());
        assert_eq!(request_key(&odd), None);
    }
}
