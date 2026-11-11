//! Calendars, events, the schedule board and its notifications (api-schedule).
//!
//! One file's tests: `cargo test -p api --test schedule <file>::`.

#[path = "../common/mod.rs"]
mod common;

mod calendars;
mod crew;
mod estimates;
mod events;
mod issues;
mod job_time;
mod schedule;
mod schedule_board;
mod schedule_notifications;
