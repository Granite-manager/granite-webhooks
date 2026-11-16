use axum::{Router, response::IntoResponse, routing::get};
use sqlx::MySqlPool;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use webhooks_core::libs::constants::OK_RESPONSE;
use webhooks_core::middleware::request_logger::print_request_body;

async fn health_check() -> impl IntoResponse {
    OK_RESPONSE
}

pub fn new_main_app(pool: MySqlPool) -> Router {
    routes().with_state(pool)
}

/// Every route, the request logger and CORS, before the pool is given:
/// `main` builds them while the secrets load. At a cold start the first
/// build is mostly the binary's pages being read in.
pub fn routes() -> Router<MySqlPool> {
    Router::new()
        .route("/", get(health_check))
        .merge(webhooks_leads::router())
        .merge(webhooks_email::router())
        .merge(webhooks_phone::router())
        .layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::exact(
                    "https://docs.granite-manager.com".parse().unwrap(),
                ))
                .allow_methods([axum::http::Method::GET])
                .allow_headers(Any),
        )
        .layer(axum::middleware::from_fn(print_request_body))
}
