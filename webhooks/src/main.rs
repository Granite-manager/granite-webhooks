use lambda_http::{Error, tracing};
use std::env::set_var;
use std::time::Duration;
use webhooks::axum_helpers::axum_app;

#[tokio::main]
async fn main() -> Result<(), Error> {
    // The routes need neither the secrets nor the database: they are built on
    // another thread while those answer over the network. At a cold start
    // their first build (~20 ms) is mostly the binary's pages being read in.
    let routes = std::thread::spawn(axum_app::routes);
    // Here, not where it is reported: a new commit then rebuilds only this.
    common::release::set(option_env!("GIT_SHA"));
    tracing::init_default_subscriber();
    common::amazon::secrets::load_into_env().await?;
    unsafe {
        set_var("AWS_LAMBDA_HTTP_IGNORE_STAGE_IN_PATH", "true");
    }
    let url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL must be set")?;
    let pool = common::db::lambda_pool(common::db::connect_options(&url)?);
    // The connection is opened now (TLS and sign-in, ~25 ms at a cold
    // start): the first request needs it at once, for its access token, and
    // nothing else is left to overlap it with. Best effort: a failure (a full
    // database) is left to the first query, which answers 500, where it used
    // to stop the Lambda.
    let _ = tokio::time::timeout(Duration::from_secs(3), pool.acquire()).await;
    let app = routes.join().expect("the routes build").with_state(pool);
    webhooks::run(app).await
}
