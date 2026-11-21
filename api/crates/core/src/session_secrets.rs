//! `SESSION_SECRET` and replacing it without signing anyone out.
//!
//! `SESSION_SECRET` signs everything the API signs: the `__session` and
//! `__customer_view` cookies, the phones' call keys, email attachment tokens,
//! survey links, the call-recording cache keys, agent tokens, web links,
//! OAuth state and customer-view codes. Only it ever signs.
//!
//! `SESSION_SECRET_OTHER`, when set, is also accepted. A rotation is two
//! secret changes, each followed by a restart of `granite-api` and
//! `granite-api-stream` (a browser talks to both, so neither may refuse what
//! the other signed while they switch one after the other):
//!
//! 1. `SESSION_SECRET_OTHER` = the new secret. Every instance now accepts
//!    both, and still signs with the old one.
//! 2. `SESSION_SECRET` = the new secret, `SESSION_SECRET_OTHER` = the old one,
//!    `SESSION_SECRET_SWITCHED_AT` = unix seconds a few minutes after the
//!    swap (by when every instance signs with the new one).
//!
//! After the switch the old secret still opens what lives long, for as long
//! as it stays set: session cookies (`api::upgrade_session_cookies` signs them
//! again for the request, and `GET /v1/auth/session` gives the browser the
//! new cookie), call keys of phones signed in before the switch (the app gets
//! its key only at sign-in), attachment tokens in saved drafts, survey links
//! in sent emails, cached call recordings and `s:` two-step secrets. What
//! lives minutes ([`SessionSecrets::recent`]) it opens for
//! [`SHORT_LIVED_GRACE_SECS`] after the switch only. Without
//! `SESSION_SECRET_SWITCHED_AT` (step 1) it opens everything.

use std::sync::Arc;

/// How long after the switch the other secret still opens short-lived
/// values: longer than the longest of them lives (agent tokens, 30 minutes).
pub const SHORT_LIVED_GRACE_SECS: i64 = 60 * 60;

/// The secret that signs, and the one also accepted during a rotation.
#[derive(Clone)]
pub struct SessionSecrets {
    current: Arc<str>,
    other: Option<Arc<str>>,
    switched_at: Option<i64>,
}

impl SessionSecrets {
    /// Only `current`: nothing else is accepted.
    pub fn new(current: &str) -> Self {
        Self {
            current: Arc::from(current),
            other: None,
            switched_at: None,
        }
    }

    /// Also accept `other` (ignored when empty or the same as the current
    /// one); `switched_at` is when `SESSION_SECRET` became the current one.
    #[must_use]
    pub fn with_other(mut self, other: &str, switched_at: Option<i64>) -> Self {
        let other = other.trim();
        self.other = (!other.is_empty() && other != &*self.current).then(|| Arc::from(other));
        self.switched_at = switched_at;
        self
    }

    /// From `SESSION_SECRET_OTHER` and `SESSION_SECRET_SWITCHED_AT`.
    #[must_use]
    pub fn with_other_from_env(self) -> Self {
        let other = std::env::var("SESSION_SECRET_OTHER").unwrap_or_default();
        let switched_at = std::env::var("SESSION_SECRET_SWITCHED_AT")
            .ok()
            .and_then(|at| at.trim().parse().ok());
        self.with_other(&other, switched_at)
    }

    /// The secret that signs.
    pub fn current(&self) -> &str {
        &self.current
    }

    /// The secret also accepted, when there is one.
    pub fn other(&self) -> Option<&str> {
        self.other.as_deref()
    }

    /// When `SESSION_SECRET` became the current one, once set.
    pub const fn switched_at(&self) -> Option<i64> {
        self.switched_at
    }

    /// The secrets a long-lived value may be signed with, the current one
    /// first.
    pub fn all(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.current()).chain(self.other())
    }

    /// The secrets a short-lived value (minutes) may be signed with at `now`
    /// (unix seconds): the other one only until [`SHORT_LIVED_GRACE_SECS`]
    /// after the switch.
    pub fn recent(&self, now: i64) -> impl Iterator<Item = &str> {
        let other = self.other().filter(|_| {
            self.switched_at
                .is_none_or(|at| now < at.saturating_add(SHORT_LIVED_GRACE_SECS))
        });
        std::iter::once(self.current()).chain(other)
    }

    /// The secrets the call key of a device session created at `created_at`
    /// (unix seconds) may come from: the other one only for a session
    /// signed in before the switch (its phone got its key then).
    pub fn for_device_session(&self, created_at: i64) -> impl Iterator<Item = &str> {
        let other = self
            .other()
            .filter(|_| self.switched_at.is_none_or(|at| created_at < at));
        std::iter::once(self.current()).chain(other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rotated() -> SessionSecrets {
        SessionSecrets::new("new").with_other("old", Some(1_000))
    }

    #[test]
    fn alone_it_accepts_only_itself() {
        let secrets = SessionSecrets::new("only").with_other("  ", Some(1));
        assert_eq!(secrets.all().collect::<Vec<_>>(), ["only"]);
        let same = SessionSecrets::new("only").with_other("only", None);
        assert_eq!(same.all().collect::<Vec<_>>(), ["only"]);
    }

    #[test]
    fn long_lived_values_open_with_both() {
        assert_eq!(rotated().all().collect::<Vec<_>>(), ["new", "old"]);
    }

    #[test]
    fn short_lived_values_open_with_the_old_one_for_an_hour() {
        let secrets = rotated();
        let at = 1_000 + SHORT_LIVED_GRACE_SECS;
        assert_eq!(secrets.recent(at - 1).collect::<Vec<_>>(), ["new", "old"]);
        assert_eq!(secrets.recent(at).collect::<Vec<_>>(), ["new"]);
    }

    #[test]
    fn old_call_keys_only_for_phones_signed_in_before() {
        let secrets = rotated();
        assert_eq!(
            secrets.for_device_session(999).collect::<Vec<_>>(),
            ["new", "old"]
        );
        assert_eq!(
            secrets.for_device_session(1_000).collect::<Vec<_>>(),
            ["new"]
        );
    }

    #[test]
    fn before_the_switch_the_next_one_opens_everything() {
        let secrets = SessionSecrets::new("old").with_other("new", None);
        assert_eq!(secrets.recent(i64::MAX).count(), 2);
        assert_eq!(secrets.for_device_session(i64::MAX).count(), 2);
    }
}
