//! The API's MySQL pool, sized for Lambda (shared with webhooks: see
//! [`common::db::lambda_pool`]).

pub use common::db::lambda_pool;
