//! Re-encrypts every value stored under `AES_KEY` with a new key.
//!
//! ```sh
//! TARGET_DATABASE_URL='mysql://admin:...@host:3306/main' \
//! OLD_AES_KEY=<64 hex> NEW_AES_KEY=<64 hex> \
//!   cargo run -q -p api --example rotate_aes_key -- [--apply]
//! ```
//!
//! `TARGET_DATABASE_URL`, not `DATABASE_URL`: `.cargo/config.toml` pins that
//! one to the local database. Without `--apply` nothing is written; the run
//! only reports what it would change.
//!
//! Encrypted columns (`base64(iv | tag | ciphertext)`, see
//! `common::telephony::ringcentral_auth::encrypt`):
//! - `company`: `ringcentral_access_token`, `ringcentral_refresh_token`,
//!   `qbo_access_token`, `qbo_refresh_token`, `qbo_client_id`,
//!   `qbo_client_secret`
//! - `ringcentral_user_logins`: `access_token`, `refresh_token`
//!
//! Each value is opened with the old key and sealed again with the new one,
//! all in one transaction with the rows locked. A value that already opens
//! with the new key is left as it is, so running again is safe. A value that
//! opens with neither key is left untouched and listed: the app cannot read
//! it either.
//!
//! Switching a deployment to the new key:
//! 1. `openssl rand -hex 32` for the new key.
//! 2. Run without `--apply`, then with it.
//! 3. Set `AES_KEY` to the new key in every secret that holds it and restart
//!    the Lambdas. Until they restart they cannot open the moved values:
//!    `QuickBooks` and `RingCentral` read as not connected, nothing is deleted.
//! 4. Run with `--apply` again: it moves anything the old Lambdas wrote
//!    under the old key in between (a new connection, for example).
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::telephony::ringcentral_auth::encrypt;
use sqlx::Row;

/// Each table with encrypted columns: name, primary key, columns.
const TABLES: &[(&str, &str, &[&str])] = &[
    (
        "company",
        "id",
        &[
            "ringcentral_access_token",
            "ringcentral_refresh_token",
            "qbo_access_token",
            "qbo_refresh_token",
            "qbo_client_id",
            "qbo_client_secret",
        ],
    ),
    (
        "ringcentral_user_logins",
        "user_id",
        &["access_token", "refresh_token"],
    ),
];

/// What one stored value needs.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// Opened with the old key; this is the value sealed with the new one.
    Move(String),
    /// Already opens with the new key.
    Done,
    /// Empty or whitespace: nothing to re-encrypt.
    Empty,
    /// Opens with neither key.
    Unreadable,
}

fn parse_key(name: &str) -> [u8; 32] {
    let hex = std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"));
    let hex = hex.trim();
    assert!(hex.len() == 64, "{name} must be 64 hex digits");
    let mut key = [0u8; 32];
    for (index, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
            .unwrap_or_else(|_| panic!("{name} must be 64 hex digits"));
    }
    key
}

/// The plaintext exactly as stored (the app's `decrypt` also trims it).
fn open(key: &[u8; 32], blob: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(blob).ok()?.trim();
    let bytes = STANDARD.decode(text).ok()?;
    if bytes.len() < 28 {
        return None;
    }
    let (iv, rest) = bytes.split_at(12);
    let (tag, body) = rest.split_at(16);
    let mut sealed = body.to_vec();
    sealed.extend_from_slice(tag);
    let plain = Aes256Gcm::new(key.into())
        .decrypt(Nonce::from_slice(iv), sealed.as_ref())
        .ok()?;
    String::from_utf8(plain).ok()
}

fn plan(old: &[u8; 32], new: &[u8; 32], blob: &[u8]) -> Plan {
    if std::str::from_utf8(blob).is_ok_and(|text| text.trim().is_empty()) {
        return Plan::Empty;
    }
    if open(new, blob).is_some() {
        return Plan::Done;
    }
    let Some(plain) = open(old, blob) else {
        return Plan::Unreadable;
    };
    let sealed = encrypt(new, &plain);
    assert_eq!(
        open(new, sealed.as_bytes()).as_deref(),
        Some(plain.as_str()),
        "a re-encrypted value must open with the new key"
    );
    Plan::Move(sealed)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), sqlx::Error> {
    let apply = std::env::args().skip(1).any(|arg| arg == "--apply");
    let url = std::env::var("TARGET_DATABASE_URL").expect("TARGET_DATABASE_URL is not set");
    let old = parse_key("OLD_AES_KEY");
    let new = parse_key("NEW_AES_KEY");
    assert!(old != new, "OLD_AES_KEY and NEW_AES_KEY are the same");

    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(common::db::connect_options(&url)?)
        .await?;
    let mut tx = pool.begin().await?;
    let (mut moved, mut done, mut unreadable) = (0usize, 0usize, Vec::new());

    for (table, id, columns) in TABLES {
        let list = columns.join(", ");
        let any_set = columns
            .iter()
            .map(|column| format!("{column} IS NOT NULL"))
            .collect::<Vec<_>>()
            .join(" OR ");
        let select = format!("SELECT {id}, {list} FROM {table} WHERE {any_set} FOR UPDATE");
        let rows = sqlx::query(sqlx::AssertSqlSafe(select))
            .fetch_all(&mut *tx)
            .await?;
        for row in rows {
            let row_id: i32 = row.try_get(*id)?;
            for column in *columns {
                let Some(blob) = row.try_get::<Option<Vec<u8>>, _>(*column)? else {
                    continue;
                };
                match plan(&old, &new, &blob) {
                    Plan::Move(sealed) => {
                        moved += 1;
                        if apply {
                            let update = format!("UPDATE {table} SET {column} = ? WHERE {id} = ?");
                            sqlx::query(sqlx::AssertSqlSafe(update))
                                .bind(sealed.into_bytes())
                                .bind(row_id)
                                .execute(&mut *tx)
                                .await?;
                        }
                    }
                    Plan::Done => done += 1,
                    Plan::Empty => {}
                    Plan::Unreadable => unreadable.push(format!("{table}.{column} {id}={row_id}")),
                }
            }
        }
    }

    if apply {
        tx.commit().await?;
    } else {
        tx.rollback().await?;
    }
    let verb = if apply {
        "re-encrypted"
    } else {
        "would re-encrypt"
    };
    println!("{verb}: {moved}");
    println!("already under the new key: {done}");
    println!("opens with neither key (left as is): {}", unreadable.len());
    for value in &unreadable {
        println!("  {value}");
    }
    if !apply && moved > 0 {
        println!("dry run: nothing written; pass --apply to write");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: [u8; 32] = [1; 32];
    const NEW: [u8; 32] = [2; 32];

    #[test]
    fn moves_a_value_from_the_old_key() {
        let blob = encrypt(&OLD, "refresh-token");
        let Plan::Move(sealed) = plan(&OLD, &NEW, blob.as_bytes()) else {
            panic!("expected a move");
        };
        assert_eq!(
            open(&NEW, sealed.as_bytes()).as_deref(),
            Some("refresh-token")
        );
        assert!(open(&OLD, sealed.as_bytes()).is_none());
    }

    #[test]
    fn keeps_the_plaintext_exactly() {
        let blob = encrypt(&OLD, " spaced ");
        let Plan::Move(sealed) = plan(&OLD, &NEW, blob.as_bytes()) else {
            panic!("expected a move");
        };
        assert_eq!(open(&NEW, sealed.as_bytes()).as_deref(), Some(" spaced "));
    }

    #[test]
    fn leaves_values_already_under_the_new_key() {
        let blob = encrypt(&NEW, "access");
        assert_eq!(plan(&OLD, &NEW, blob.as_bytes()), Plan::Done);
    }

    #[test]
    fn reports_values_neither_key_opens() {
        let blob = encrypt(&[3; 32], "access");
        assert_eq!(plan(&OLD, &NEW, blob.as_bytes()), Plan::Unreadable);
        assert_eq!(plan(&OLD, &NEW, b"not base64!"), Plan::Unreadable);
    }

    #[test]
    fn skips_empty_values() {
        assert_eq!(plan(&OLD, &NEW, b""), Plan::Empty);
        assert_eq!(plan(&OLD, &NEW, b"  "), Plan::Empty);
    }
}
