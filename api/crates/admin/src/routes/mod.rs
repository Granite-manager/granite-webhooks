//! Route modules of this crate. The ones of the crates below are
//! re-exported, so every route module is at `crate::routes::<name>`.

pub use api_accounts::routes::*;
pub use api_schedule::routes::*;

pub mod billing;
pub mod billing_account;
pub mod bug_reports;
pub mod checklists;
pub mod company_frame;
pub mod company_setup;
pub mod customer_sources;
pub mod dashboard;
pub mod delayed;
pub mod first_payment;
pub mod integrations;
pub mod maintenance;
pub mod marketing;
pub mod paid_features;
pub mod payments;
pub mod position_sections;
pub mod posthog_autofix;
pub mod questionnaires;
pub mod reports;
pub mod signup;
pub mod superuser;
pub mod superuser_companies;
pub mod superuser_ringcentral;
pub mod survey_results;
pub mod teach_progress;
