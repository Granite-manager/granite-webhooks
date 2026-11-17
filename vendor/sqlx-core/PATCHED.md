# sqlx-core 0.9.0, patched

This is `sqlx-core` 0.9.0 from crates.io with one change, in
`src/net/socket/mod.rs` (`connect_tcp`): the tokio TCP socket gets
`TCP_NODELAY`.

sqlx 0.9.0 leaves Nagle's algorithm on. During the TLS handshake with MySQL
the client makes small writes back to back, and each one after the first
waits for the server's delayed ACK (about 40 ms on Linux). From a Lambda in
us-east-2, opening a TLS connection to RDS took about 100 ms with it and
5-10 ms without it (measured Nov 2026; without TLS it took 2-4 ms either
way). Every cold start and every reconnect of the api and webhooks Lambdas
paid it.

The root `Cargo.toml` points `[patch.crates-io]` here. To drop the patch:
remove that entry and this folder once a sqlx release sets `TCP_NODELAY`
itself (check `connect_tcp` in `sqlx-core/src/net/socket/mod.rs`), or when
upgrading sqlx past 0.9.0 (a patch for another version is ignored, with a
warning from cargo).
