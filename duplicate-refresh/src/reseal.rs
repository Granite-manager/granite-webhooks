//! Two-step sign-in secrets (`user_mfa.secret`, `user_mfa.pending_secret`)
//! are encrypted with the API's key: `a:` + AES-GCM under `AES_KEY`, or `s:`
//! under the SHA-256 of `SESSION_SECRET` where `AES_KEY` is not set (see
//! `api/crates/accounts/src/mfa.rs`). Production and dev have different
//! keys, so a copied secret does not open on dev and nobody with two-step
//! sign-in could sign in there.
//!
//! The copy re-encrypts them, in staging before the swap, from production's
//! key to dev's: the same authenticator code then works on both. Dev never
//! gets production's `AES_KEY`, which also opens the Gmail, QuickBooks and
//! RingCentral tokens copied into `duplicate`.

use std::collections::BTreeMap;

use common::telephony::ringcentral_auth::{decrypt, encrypt, parse_hex_key};
use lambda_runtime::tracing;
use sha2::{Digest, Sha256};
use sqlx::{AssertSqlSafe, MySqlConnection};

/// One API's keys, from its secret.
pub struct Keys {
    aes: Option<[u8; 32]>,
    session: [u8; 32],
}

impl Keys {
    pub fn from_secret(name: &str, pairs: &BTreeMap<String, String>) -> Result<Self, String> {
        let session = pairs
            .get("SESSION_SECRET")
            .ok_or_else(|| format!("{name} has no SESSION_SECRET"))?;
        let aes = match pairs.get("AES_KEY") {
            None => None,
            Some(hex) => Some(
                parse_hex_key(hex.trim())
                    .ok_or_else(|| format!("{name}: AES_KEY is not 64 hex digits"))?,
            ),
        };
        Ok(Self {
            aes,
            session: Sha256::digest(session.as_bytes()).into(),
        })
    }

    /// As the API's `mfa::open`.
    fn open(&self, stored: &str) -> Option<String> {
        let (tag, sealed) = stored.split_at_checked(2)?;
        let key = match tag {
            "a:" => self.aes.as_ref()?,
            "s:" => &self.session,
            _ => return None,
        };
        decrypt(key, sealed.as_bytes())
    }

    /// As the API's `mfa::seal`.
    fn seal(&self, plain: &str) -> String {
        match &self.aes {
            Some(key) => format!("a:{}", encrypt(key, plain)),
            None => format!("s:{}", encrypt(&self.session, plain)),
        }
    }
}

pub struct Reseal {
    pub source: Keys,
    pub target: Keys,
}

impl Reseal {
    /// `stored` encrypted for the target; `None` when the source's keys do
    /// not open it.
    fn convert(&self, stored: &str) -> Option<String> {
        self.source
            .open(stored)
            .map(|plain| self.target.seal(&plain))
    }

    /// Re-encrypts `schema.user_mfa` (the staging copy) for the target. A
    /// value the source's keys do not open is left as it is.
    pub async fn apply(
        &self,
        conn: &mut MySqlConnection,
        schema: &str,
        quoted: &str,
    ) -> Result<(), String> {
        let exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = ? AND table_name = 'user_mfa'",
        )
        .bind(schema)
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| format!("user_mfa: {e}"))?;
        if exists == 0 {
            return Ok(());
        }
        let rows: Vec<(i32, Option<String>, Option<String>)> =
            sqlx::query_as(AssertSqlSafe(format!(
                "SELECT user_id, secret, pending_secret FROM {quoted}.user_mfa
             WHERE secret IS NOT NULL OR pending_secret IS NOT NULL"
            )))
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| format!("user_mfa: {e}"))?;
        let (mut done, mut unopened) = (0, 0);
        for (user_id, secret, pending) in rows {
            let mut convert = |value: Option<String>| {
                value.map(|stored| {
                    self.convert(&stored).unwrap_or_else(|| {
                        unopened += 1;
                        stored
                    })
                })
            };
            let (secret, pending) = (convert(secret), convert(pending));
            sqlx::query(AssertSqlSafe(format!(
                "UPDATE {quoted}.user_mfa SET secret = ?, pending_secret = ? WHERE user_id = ?"
            )))
            .bind(secret)
            .bind(pending)
            .bind(user_id)
            .execute(&mut *conn)
            .await
            .map_err(|e| format!("user_mfa {user_id}: {e}"))?;
            done += 1;
        }
        tracing::info!(
            users = done,
            unopened,
            "re-encrypted two-step sign-in secrets for dev"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(aes: Option<&str>, session: &str) -> Keys {
        let mut pairs = BTreeMap::new();
        pairs.insert("SESSION_SECRET".to_string(), session.to_string());
        if let Some(aes) = aes {
            pairs.insert("AES_KEY".to_string(), aes.to_string());
        }
        Keys::from_secret("test", &pairs).unwrap()
    }

    const PROD_AES: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    #[test]
    fn a_production_secret_opens_on_dev_after_resealing() {
        let prod = keys(Some(PROD_AES), "prod-session");
        let dev = keys(None, "dev-session");
        let stored = prod.seal("JBSWY3DPEHPK3PXP");
        assert!(stored.starts_with("a:"));
        assert_eq!(dev.open(&stored), None);

        let reseal = Reseal {
            source: prod,
            target: dev,
        };
        let moved = reseal.convert(&stored).unwrap();
        assert!(moved.starts_with("s:"));
        assert_eq!(
            reseal.target.open(&moved).as_deref(),
            Some("JBSWY3DPEHPK3PXP")
        );
        assert_eq!(reseal.source.open(&moved), None);
    }

    #[test]
    fn session_sealed_secrets_move_too() {
        let prod = keys(Some(PROD_AES), "prod-session");
        let stored = format!(
            "s:{}",
            encrypt(&Sha256::digest(b"prod-session").into(), "ABC")
        );
        let reseal = Reseal {
            source: prod,
            target: keys(None, "dev-session"),
        };
        let moved = reseal.convert(&stored).unwrap();
        assert_eq!(reseal.target.open(&moved).as_deref(), Some("ABC"));
    }

    #[test]
    fn unknown_values_do_not_convert() {
        let reseal = Reseal {
            source: keys(None, "a"),
            target: keys(None, "b"),
        };
        assert_eq!(reseal.convert("x:abc"), None);
        assert_eq!(reseal.convert("a:abc"), None);
    }

    #[test]
    fn a_bad_aes_key_is_an_error() {
        let mut pairs = BTreeMap::new();
        pairs.insert("SESSION_SECRET".to_string(), "s".to_string());
        pairs.insert("AES_KEY".to_string(), "short".to_string());
        assert!(Keys::from_secret("x", &pairs).is_err());
        assert!(Keys::from_secret("x", &BTreeMap::new()).is_err());
    }
}
