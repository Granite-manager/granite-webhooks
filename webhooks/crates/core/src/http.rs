//! HTTP clients this Lambda instance reuses between the calls of a request
//! and the requests that follow it closely.
//!
//! A new client opens a new connection (DNS, TCP and TLS) to each host. From
//! us-east-2 that added about 10 ms to a call to the api, `RingCentral`,
//! Google or Telnyx, and about 190 ms to one to Telegram (measured Nov 2026;
//! Telegram's servers are in Europe). A [`Reused`] client is handed out again
//! while the last hand-out was less than [`REUSE_WITHIN`] ago by the wall
//! clock, so its connections are reused; after a longer pause a new client
//! is built.
//!
//! Why the wall clock: Lambda freezes the instance between requests, and
//! the clock the connection pool goes by (`POOL_IDLE` in `common::http`)
//! stops meanwhile, so a connection the server closed during a long freeze
//! would look fresh. The wall clock is right again after a freeze: after a
//! pause longer than [`REUSE_WITHIN`], frozen or not, nothing is reused. The
//! servers above keep an idle connection open for longer than that.
//!
//! The tests build a new client every time: each test runs its own tokio
//! runtime, and a connection opened on another one is not used across them.

use reqwest::Client;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A client is handed out again within this long of its last hand-out.
pub const REUSE_WITHIN: Duration = Duration::from_secs(15);

/// A client built by `build`, reused as the module says.
pub struct Reused<T: Clone> {
    build: fn() -> T,
    /// The client and when it was last handed out (wall clock, ms).
    slot: Mutex<Option<(T, u128)>>,
}

impl<T: Clone> Reused<T> {
    pub const fn new(build: fn() -> T) -> Self {
        Self {
            build,
            slot: Mutex::new(None),
        }
    }

    /// The client: the last one when it was handed out within
    /// [`REUSE_WITHIN`], else a new one.
    pub fn get(&self) -> T {
        if cfg!(any(test, feature = "test-support")) {
            return (self.build)();
        }
        self.get_at(wall_ms())
    }

    /// [`Self::get`] at `now` (wall clock, ms).
    fn get_at(&self, now: u128) -> T {
        let Ok(mut slot) = self.slot.lock() else {
            return (self.build)();
        };
        match slot.as_mut() {
            Some((client, handed_out))
                if now.saturating_sub(*handed_out) < REUSE_WITHIN.as_millis() =>
            {
                *handed_out = now;
                client.clone()
            }
            _ => {
                let client = (self.build)();
                *slot = Some((client.clone(), now));
                client
            }
        }
    }
}

fn wall_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis())
}

static SHARED: Reused<Client> = Reused::new(common::http::client);

/// `common::http::client()`, reused (see the module).
pub fn client() -> Client {
    SHARED.get()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn counter() -> usize {
        static BUILT: AtomicUsize = AtomicUsize::new(0);
        BUILT.fetch_add(1, Ordering::Relaxed)
    }

    #[test]
    fn a_client_is_reused_until_a_pause() {
        let reused = Reused::new(counter);
        let within = REUSE_WITHIN.as_millis() - 1;
        let first = reused.get_at(1_000);
        assert_eq!(reused.get_at(1_000 + within), first);
        // Measured from the last hand-out, not the first.
        assert_eq!(reused.get_at(1_000 + 2 * within), first);
        let after_pause = reused.get_at(1_000 + 3 * within + REUSE_WITHIN.as_millis());
        assert_ne!(after_pause, first);
        // A clock that went back does not count as a pause.
        assert_eq!(reused.get_at(500), after_pause);
    }

    #[test]
    fn tests_get_a_new_client_every_time() {
        let reused = Reused::new(counter);
        assert_ne!(reused.get(), reused.get());
    }
}
