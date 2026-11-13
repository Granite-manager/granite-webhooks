//! Raptor (LPI Cloud, templator.com) jobs from new projects.
//!
//! Each new project of a company that saved its Raptor login becomes a Raptor
//! job, filled the way the office does by hand (job info, material, sink
//! cutouts and the "Notes" note). `POST /v1/jobs/raptor-jobs` runs `sync::run`.

pub mod client;
pub mod plan;
pub mod sync;
