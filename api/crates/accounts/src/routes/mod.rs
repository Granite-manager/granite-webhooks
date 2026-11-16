//! Route modules of this crate. The ones of the crates below are
//! re-exported, so every route module is at `crate::routes::<name>`.

pub use api_crm::routes::*;

pub mod app_shell;
pub mod auth;
pub mod device_sessions;
pub mod leaderboard;
pub mod login_steps;
pub mod me;
pub mod onboarding;
pub mod password_reset_code;
pub mod teach_mode;
pub mod users;
pub mod web_link;
