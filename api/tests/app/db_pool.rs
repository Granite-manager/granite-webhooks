//! The Lambda pool must not hold connections the database can't reclaim.

use api::db::lambda_pool;
use sqlx::MySql;
use sqlx::mysql::MySqlConnectOptions;
use sqlx::pool::PoolOptions;

#[sqlx::test]
async fn lambda_pool_uses_one_short_lived_connection(
    _pool_options: PoolOptions<MySql>,
    connect_options: MySqlConnectOptions,
) {
    let pool = lambda_pool(connect_options);
    assert_eq!(pool.size(), 0, "connects lazily");

    let wait_timeout: u64 = sqlx::query_scalar("SELECT @@SESSION.wait_timeout")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(wait_timeout, 60);
    assert_eq!(pool.options().get_max_connections(), 1);

    // Parallel queries share the single connection instead of deadlocking.
    let (a, b) = tokio::join!(
        sqlx::query_scalar::<_, i64>("SELECT 1").fetch_one(&pool),
        sqlx::query_scalar::<_, i64>("SELECT 2").fetch_one(&pool),
    );
    assert_eq!((a.unwrap(), b.unwrap()), (1, 2));
    assert_eq!(pool.size(), 1);
}

/// Pings this connection has answered (`COM_PING` counts as an admin command).
async fn pings(pool: &sqlx::MySqlPool) -> i64 {
    let (_, count): (String, String) =
        sqlx::query_as("SHOW SESSION STATUS LIKE 'Com_admin_commands'")
            .fetch_one(pool)
            .await
            .unwrap();
    count.parse().unwrap()
}

#[sqlx::test]
async fn lambda_pool_pings_only_after_a_pause(
    _pool_options: PoolOptions<MySql>,
    connect_options: MySqlConnectOptions,
) {
    let pool = lambda_pool(connect_options.clone());
    let before = pings(&pool).await;
    for _ in 0..10 {
        sqlx::query_scalar::<_, i64>("SELECT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    }
    // The queries of one request follow each other: one ping each (sqlx's own,
    // as a query hands the connection back) for the count above and the ten,
    // not a second one before each.
    assert_eq!(pings(&pool).await, before + 11);

    // After a pause the connection is checked before it is used again.
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    // The eleven, the count's own, the check after the pause and the query's.
    assert_eq!(pings(&pool).await, before + 14);
}

#[sqlx::test]
async fn lambda_pool_replaces_a_connection_the_database_dropped(
    _pool_options: PoolOptions<MySql>,
    connect_options: MySqlConnectOptions,
) {
    let pool = lambda_pool(connect_options.clone());
    let first: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
        .fetch_one(&pool)
        .await
        .unwrap();
    // As MySQL does to a container's connection after 60 idle seconds.
    let other = lambda_pool(connect_options);
    sqlx::query(sqlx::AssertSqlSafe(format!("KILL {first}")))
        .execute(&other)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

    let second: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_ne!(first, second, "the next query runs on a new connection");
    assert_eq!(pool.size(), 1);
}
