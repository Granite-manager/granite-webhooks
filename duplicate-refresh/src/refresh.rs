//! One refresh: copy every table of the source into a staging database on
//! the same server, then swap the staging tables in for the target's in one
//! atomic `RENAME TABLE`.
//!
//! - The source is only read. The user may only read it ([`crate::grants`]),
//!   and every statement that writes names a target, staging or retired
//!   database, never the source.
//! - Reads of the source take no row locks: the session runs at READ
//!   COMMITTED, so each `INSERT ... SELECT` reads the source as a consistent
//!   (non-locking) snapshot, and writers on the source never wait for it.
//!   (At REPEATABLE READ, `INSERT ... SELECT` would put shared locks on every
//!   row it reads.) Each table is its own snapshot, a few seconds apart, so a
//!   row written mid-run may be missing from one table and present in
//!   another; foreign keys are not checked while copying for that reason.
//! - The target keeps working until the swap, and keeps its old tables when
//!   anything fails before it: a failed run leaves `duplicate` as it was and
//!   the next run starts over. The target database itself is never dropped,
//!   so the grants on it stay.

use std::time::Instant;

use lambda_runtime::tracing;
use sqlx::mysql::MySqlRow;
use sqlx::{AssertSqlSafe, MySqlConnection, Row};

use crate::grants;

pub struct Schemas {
    /// Read only.
    pub source: &'static str,
    /// Its tables are replaced by copies of the source's.
    pub target: &'static str,
    /// Built from scratch every run, then emptied by the swap.
    pub staging: &'static str,
    /// Holds the target's old tables between the swap and their drop.
    pub retired: &'static str,
}

impl Schemas {
    pub const PRODUCTION: Self = Self {
        source: "main",
        target: "duplicate",
        staging: "duplicate_next",
        retired: "duplicate_old",
    };
}

#[derive(Debug)]
pub struct Report {
    pub tables: usize,
    pub rows: u64,
    pub seconds: u64,
}

/// A MySQL named lock: a second run started while one is going gives up.
const LOCK_NAME: &str = "duplicate-refresh";

/// The session: non-locking reads of the source (see the module docs), no
/// foreign key checks while tables fill in any order, `0` kept as a value in
/// `AUTO_INCREMENT` columns, and no 15 s statement limit (the server's
/// `max_execution_time` applies to `SELECT`s). `lock_wait_timeout` bounds the
/// wait for a table another session holds (a migration on the source):
/// the run fails instead of waiting for it.
const SESSION: &str = "SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED;
SET SESSION foreign_key_checks = 0, sql_mode = 'NO_AUTO_VALUE_ON_ZERO', max_execution_time = 0,
    lock_wait_timeout = 60, innodb_lock_wait_timeout = 60, wait_timeout = 900,
    net_read_timeout = 600, net_write_timeout = 600";

pub async fn run(
    conn: &mut MySqlConnection,
    schemas: &Schemas,
    check_only: bool,
) -> Result<Report, String> {
    let source = schemas.source;
    if [schemas.target, schemas.staging, schemas.retired].contains(&source) {
        return Err("the source must not be written".to_string());
    }
    let grant_lines: Vec<String> = sqlx::raw_sql("SHOW GRANTS")
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| format!("SHOW GRANTS: {e}"))?
        .iter()
        .map(|row| row.try_get(0))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("SHOW GRANTS: {e}"))?;
    grants::check(&grant_lines, source)?;

    sqlx::raw_sql(SESSION)
        .execute(&mut *conn)
        .await
        .map_err(|e| format!("session settings: {e}"))?;
    let locked: Option<i64> = sqlx::query_scalar("SELECT GET_LOCK(?, 0)")
        .bind(LOCK_NAME)
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| format!("GET_LOCK: {e}"))?;
    if locked != Some(1) {
        return Err("another refresh is running".to_string());
    }
    let result = copy(conn, schemas, check_only).await;
    let _ = sqlx::query("SELECT RELEASE_LOCK(?)")
        .bind(LOCK_NAME)
        .execute(&mut *conn)
        .await;
    result
}

async fn copy(
    conn: &mut MySqlConnection,
    schemas: &Schemas,
    check_only: bool,
) -> Result<Report, String> {
    let started = Instant::now();
    let Schemas {
        source,
        target,
        staging,
        retired,
    } = *schemas;

    let source_tables = tables(conn, source).await?;
    if let Some(view) = source_tables.iter().find(|(_, kind)| kind != "BASE TABLE") {
        return Err(format!("{source}.{} is a {}: not copied", view.0, view.1));
    }
    let names: Vec<String> = source_tables.into_iter().map(|(name, _)| name).collect();
    if names.is_empty() {
        return Err(format!(
            "{source} has no tables (or the user cannot see them)"
        ));
    }
    let (charset, collation): (String, String) = sqlx::query_as(
        "SELECT default_character_set_name, default_collation_name
         FROM information_schema.schemata WHERE schema_name = ?",
    )
    .bind(source)
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| format!("{source} charset: {e}"))?;
    let create_database = |name: &str| {
        format!(
            "CREATE DATABASE {} CHARACTER SET {} COLLATE {}",
            ident(name),
            ident(&charset),
            ident(&collation)
        )
    };

    exec(conn, format!("DROP DATABASE IF EXISTS {}", ident(staging))).await?;
    exec(conn, create_database(staging)).await?;
    if check_only {
        exec(conn, format!("DROP DATABASE {}", ident(staging))).await?;
        return Ok(Report {
            tables: names.len(),
            rows: 0,
            seconds: started.elapsed().as_secs(),
        });
    }

    // `SHOW CREATE TABLE` names neither the database of the table nor (for a
    // foreign key to a table of the same database) of the parent: run in
    // staging, it makes the table and its foreign keys there.
    exec(conn, format!("USE {}", ident(staging))).await?;
    let mut rows = 0;
    for table in &names {
        let table_started = Instant::now();
        let row: MySqlRow = sqlx::raw_sql(AssertSqlSafe(format!(
            "SHOW CREATE TABLE {}.{}",
            ident(source),
            ident(table)
        )))
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| format!("SHOW CREATE TABLE {table}: {e}"))?;
        let ddl: String = row
            .try_get(1)
            .map_err(|e| format!("SHOW CREATE TABLE {table}: {e}"))?;
        if !ddl.starts_with(&format!("CREATE TABLE {}", ident(table)))
            || ddl.contains(&format!("{}.", ident(source)))
        {
            return Err(format!("unexpected definition of {source}.{table}"));
        }
        exec(conn, ddl).await?;

        // Generated columns are computed, never inserted.
        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT column_name FROM information_schema.columns
             WHERE table_schema = ? AND table_name = ? AND generation_expression = ''
             ORDER BY ordinal_position",
        )
        .bind(source)
        .bind(table)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| format!("columns of {table}: {e}"))?;
        let columns = columns
            .iter()
            .map(|c| ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        let copied = exec(
            conn,
            format!(
                "INSERT INTO {}.{t} ({columns}) SELECT {columns} FROM {}.{t}",
                ident(staging),
                ident(source),
                t = ident(table),
            ),
        )
        .await?;
        rows += copied;
        tracing::info!(
            table,
            rows = copied,
            ms = table_started.elapsed().as_millis(),
            "copied"
        );
    }

    let staged = tables(conn, staging).await?;
    if staged.len() != names.len() {
        return Err(format!(
            "{staging} has {} tables, {source} {}",
            staged.len(),
            names.len()
        ));
    }

    // The swap. Views in the target cannot move to another database; the
    // source has none, so the target keeps none.
    let old = tables(conn, target).await?;
    for (view, _) in old.iter().filter(|(_, kind)| kind == "VIEW") {
        exec(conn, format!("DROP VIEW {}.{}", ident(target), ident(view))).await?;
    }
    exec(conn, format!("DROP DATABASE IF EXISTS {}", ident(retired))).await?;
    exec(conn, create_database(retired)).await?;
    let moves: Vec<String> = old
        .iter()
        .filter(|(_, kind)| kind == "BASE TABLE")
        .map(|(t, _)| {
            format!(
                "{}.{t} TO {}.{t}",
                ident(target),
                ident(retired),
                t = ident(t)
            )
        })
        .chain(names.iter().map(|t| {
            format!(
                "{}.{t} TO {}.{t}",
                ident(staging),
                ident(target),
                t = ident(t)
            )
        }))
        .collect();
    exec(conn, format!("RENAME TABLE {}", moves.join(", "))).await?;
    tracing::info!(tables = names.len(), rows, "swapped in the new tables");

    exec(conn, format!("DROP DATABASE {}", ident(retired))).await?;
    exec(conn, format!("DROP DATABASE {}", ident(staging))).await?;
    Ok(Report {
        tables: names.len(),
        rows,
        seconds: started.elapsed().as_secs(),
    })
}

/// The tables and views of `schema`: (name, `BASE TABLE` or `VIEW`).
async fn tables(conn: &mut MySqlConnection, schema: &str) -> Result<Vec<(String, String)>, String> {
    sqlx::query_as(
        "SELECT table_name, table_type FROM information_schema.tables
         WHERE table_schema = ? ORDER BY table_name",
    )
    .bind(schema)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| format!("tables of {schema}: {e}"))
}

/// Runs one statement over the text protocol (`USE` and some DDL cannot be
/// prepared) and gives the rows it changed.
async fn exec(conn: &mut MySqlConnection, sql: String) -> Result<u64, String> {
    let shown: String = sql.chars().take(120).collect();
    sqlx::raw_sql(AssertSqlSafe(sql))
        .execute(&mut *conn)
        .await
        .map(|done| done.rows_affected())
        .map_err(|e| format!("{shown}: {e}"))
}

/// A MySQL identifier, quoted.
fn ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ident_quotes() {
        assert_eq!(ident("main"), "`main`");
        assert_eq!(ident("a`b"), "`a``b`");
    }

    #[test]
    fn production_never_writes_main() {
        let s = Schemas::PRODUCTION;
        assert_eq!(s.source, "main");
        for written in [s.target, s.staging, s.retired] {
            assert_ne!(written, "main");
        }
    }
}
