use api::app_with_routes;
use api::db::lambda_pool;
use api::posthog_autofix::Autofix;
use api::state::AppState;
use lambda_http::{Error, run, run_with_streaming_response, tracing};
use std::env::set_var;

#[tokio::main]
async fn main() -> Result<(), Error> {
    // The routes need no state: they are built on another thread while the
    // secrets load over the network. At a cold start the first build of the
    // router is mostly the binary's pages being read in (about 80 ms on a
    // 512 MB Lambda), which this overlaps with the Secrets Manager call.
    let routes = std::thread::spawn(api::openapi::router_and_spec_json);
    // Here, not where it is reported: a new commit then rebuilds only this.
    common::release::set(option_env!("GIT_SHA"));
    tracing::init_default_subscriber();
    common::amazon::secrets::load_into_env().await?;
    unsafe {
        set_var("AWS_LAMBDA_HTTP_IGNORE_STAGE_IN_PATH", "true");
    }
    let url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL must be set")?;
    let pool = lambda_pool(common::db::connect_options(&url)?);
    // Opens the database connection while the rest of the startup runs, so
    // the first request does not wait the ~100 ms (TLS and sign-in) alone.
    // Best effort: a failure (a full database) is left to the first query,
    // which answers it as before, and the timeout frees the pool's one slot
    // if Lambda froze the container halfway (an environment started ahead).
    let warm = pool.clone();
    tokio::spawn(async move {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), warm.acquire()).await;
    });
    // The PostHog auto-fix settings live in a crate above `AppState`'s.
    let state = AppState::from_env(pool)?.with_extension(Autofix::from_env());
    let app = app_with_routes(state, routes.join().expect("the routes build"));
    // The same API behind a Function URL in RESPONSE_STREAM mode, for the
    // answers that stream (the AI chat) or outlast API Gateway's 30 s.
    if std::env::var("LAMBDA_RESPONSE_STREAMING").as_deref() == Ok("1") {
        return run_with_streaming_response(app).await;
    }
    run(app).await
}
