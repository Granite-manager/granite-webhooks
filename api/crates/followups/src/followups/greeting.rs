//! Whether the next message greets the customer by name.
//!
//! One rule for every AI-written SMS and email (the AI Reply button, inline
//! autocomplete, follow-ups): greet by name unless the rep's side already
//! wrote to this customer (SMS or email) earlier the same store-local
//! (Indianapolis) day. Automatic texts and emails (reminders, "on the way")
//! do not count either. The customer writing first today does not count: the
//! first reply of the day still opens with "Hi <name>," (owner, 2026-11-16).
//! Once we have written today the conversation is still going: no greeting
//! and no name, the message starts with its content. Either way the name
//! appears at most once.

use super::crm_hold::parse_followup_timeline_at;
use super::dates::Instant;
use super::prompt::FollowupTimelineMessage;
use super::store_time::store_zoned_parts;
use super::timeline::Direction;
use chrono::Duration;

/// How recent the conversation with this customer is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationRecency {
    /// No dated SMS or email with them yet: greet by name.
    NoHistory,
    /// We already wrote to them today: no greeting, no name.
    Continuing {
        /// Since the newest message, either way, on any channel.
        minutes_since_last: i64,
    },
    /// We have not written to them yet today: greet by name.
    Reopening {
        /// Since the newest message, either way, on any channel.
        minutes_since_last: i64,
    },
}

impl ConversationRecency {
    /// No greeting and no name.
    pub const fn is_continuing(self) -> bool {
        matches!(self, Self::Continuing { .. })
    }
}

/// How recent the conversation is at `now`, from every dated message.
pub fn conversation_recency(
    timeline: &[FollowupTimelineMessage],
    now: Instant,
) -> ConversationRecency {
    let dated: Vec<(Instant, Direction, bool)> = timeline
        .iter()
        .filter_map(|m| parse_followup_timeline_at(&m.at).map(|at| (at, m.direction, m.automated)))
        .collect();
    let Some(last) = dated.iter().map(|(at, ..)| *at).max() else {
        return ConversationRecency::NoHistory;
    };
    let today = store_zoned_parts(now);
    // A message stamped a little ahead of the server clock is "just now".
    let is_today = |at: Instant| {
        let day = store_zoned_parts(at);
        at >= now || (day.year, day.month, day.day) == (today.year, today.month, today.day)
    };
    let minutes_since_last = (now - last).max(Duration::zero()).num_minutes();
    // An automatic text (a reminder, "the crew is on the way") is not the
    // rep writing: the rep's first message still greets by name.
    let we_wrote_today = dated
        .iter()
        .any(|(at, dir, automated)| *dir == Direction::Outbound && !automated && is_today(*at));
    if we_wrote_today {
        ConversationRecency::Continuing { minutes_since_last }
    } else {
        ConversationRecency::Reopening { minutes_since_last }
    }
}

/// "12 minutes ago", "about 5 hours ago", "3 days ago".
pub fn describe_ago(minutes: i64) -> String {
    match minutes {
        m if m < 2 => "just now".into(),
        m if m < 60 => format!("{m} minutes ago"),
        m if m < 120 => "about an hour ago".into(),
        m if m < 48 * 60 => format!("about {} hours ago", m / 60),
        m => format!("{} days ago", m / (24 * 60)),
    }
}

/// The greeting and name rule for a drafted message, as a prompt line.
pub fn greeting_instruction(recency: ConversationRecency) -> String {
    match recency {
        ConversationRecency::Continuing { minutes_since_last } => format!(
            "GREETING: NONE. This conversation is ongoing today: the rep already wrote to this customer today, and the last message with them (SMS or email, either side) was {}. Do NOT open with a greeting (\"Hi\", \"Hey\", \"Hello\", \"Good morning\", \"Dear\") and do NOT use the customer's name anywhere in the message. Start straight with the content, like the next text in a running thread. This applies to email too, even when the earlier messages were texts.",
            describe_ago(minutes_since_last)
        ),
        ConversationRecency::Reopening { minutes_since_last } => format!(
            "GREETING: BY NAME. The last message with this customer was {}, and the rep has not written to them yet today, so open with a short greeting and their first name (\"Hi Ann,\"), then get straight to the point. This holds even when the customer wrote earlier today. Use their name only that once. Never \"Hello <Full Name>\" or \"Dear\".",
            describe_ago(minutes_since_last)
        ),
        ConversationRecency::NoHistory => "GREETING: BY NAME. No earlier SMS or email with this customer, so open with a short greeting and their first name (\"Hi Ann,\"). Use their name only that once.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::followups::send_window::FollowupChannel;
    use crate::followups::timeline::Direction;
    use Direction::{Inbound as I, Outbound as O};
    use FollowupChannel::{Email as E, Sms as S};

    fn message(
        at: &str,
        channel: FollowupChannel,
        direction: Direction,
    ) -> FollowupTimelineMessage {
        FollowupTimelineMessage {
            at: at.into(),
            channel,
            direction,
            text: "x".into(),
            from_email: None,
            from_phone: None,
            automated: false,
        }
    }

    fn at(iso: &str) -> Instant {
        chrono::DateTime::parse_from_rfc3339(iso).unwrap().to_utc()
    }

    #[test]
    fn a_new_conversation_greets_by_name() {
        let now = at("2026-10-14T15:00:00Z");
        assert_eq!(
            conversation_recency(&[], now),
            ConversationRecency::NoHistory
        );
        // An email without a send time is not a dated message.
        assert_eq!(
            conversation_recency(&[message("", E, O)], now),
            ConversationRecency::NoHistory
        );
        let line = greeting_instruction(ConversationRecency::NoHistory);
        assert!(line.starts_with("GREETING: BY NAME") && line.contains("only that once"));
    }

    #[test]
    fn a_text_just_sent_means_no_greeting_or_name_on_the_email() {
        // The rep texted her 20 minutes ago and now writes an email.
        let timeline = [
            message("2026-09-01 10:00", E, O),
            message("2026-10-14 14:40", S, O),
        ];
        let recency = conversation_recency(&timeline, at("2026-10-14T15:00:00Z"));
        assert_eq!(
            recency,
            ConversationRecency::Continuing {
                minutes_since_last: 20
            }
        );
        let line = greeting_instruction(recency);
        assert!(line.starts_with("GREETING: NONE"));
        assert!(line.contains("20 minutes ago"));
        assert!(line.contains("do NOT use the customer's name"));
        assert!(line.contains("applies to email too"));
    }

    #[test]
    fn an_automatic_text_today_still_gets_a_greeting() {
        // "The crew is on the way" went out at 8:29 AM; the rep has not written yet.
        let timeline = [
            message("2026-11-16 19:49", E, O),
            FollowupTimelineMessage {
                automated: true,
                ..message("2026-11-18 13:29", S, O)
            },
            message("2026-11-18 15:19", E, I),
        ];
        let recency = conversation_recency(&timeline, at("2026-11-18T15:30:00Z"));
        assert_eq!(
            recency,
            ConversationRecency::Reopening {
                minutes_since_last: 11
            }
        );
        assert!(greeting_instruction(recency).starts_with("GREETING: BY NAME"));
    }

    #[test]
    fn the_customer_writing_first_today_still_gets_a_greeting() {
        // Carolyn emailed at 7:43 AM; the rep's first reply of the day greets.
        let timeline = [
            message("2026-11-10 15:00", E, O),
            message("2026-11-16 12:43", E, I),
        ];
        let recency = conversation_recency(&timeline, at("2026-11-16T14:00:00Z"));
        assert_eq!(
            recency,
            ConversationRecency::Reopening {
                minutes_since_last: 77
            }
        );
        assert!(greeting_instruction(recency).starts_with("GREETING: BY NAME"));
        // Our text to her earlier today: the email continues without "Hi".
        let mut replied = timeline.to_vec();
        replied.push(message("2026-11-16 13:00", S, O));
        assert!(conversation_recency(&replied, at("2026-11-16T14:00:00Z")).is_continuing());
    }

    #[test]
    fn only_our_message_from_today_continues() {
        // Either channel counts; the store day is Indianapolis (UTC-4 in October).
        for last in [
            message("2026-10-14 13:00", S, O),
            message("2026-10-14 13:00", E, O),
        ] {
            let timeline = [message("2026-01-02 10:00", S, O), last];
            // 11:59 PM the same store day: still today.
            assert!(conversation_recency(&timeline, at("2026-10-15T03:59:00Z")).is_continuing());
            // Just after store midnight: a new day, greet.
            assert_eq!(
                conversation_recency(&timeline, at("2026-10-15T04:01:00Z")),
                ConversationRecency::Reopening {
                    minutes_since_last: 901
                }
            );
        }
    }

    #[test]
    fn last_evening_is_a_new_day_greeting() {
        // 9:30 PM in Indiana is 01:30 UTC the next day; the next morning greets.
        let timeline = [message("2026-10-14 01:30", S, I)];
        let recency = conversation_recency(&timeline, at("2026-10-14T13:00:00Z"));
        assert_eq!(
            recency,
            ConversationRecency::Reopening {
                minutes_since_last: 690
            }
        );
        let line = greeting_instruction(recency);
        assert!(line.starts_with("GREETING: BY NAME") && line.contains("about 11 hours ago"));
    }

    #[test]
    fn a_few_days_later_greets_by_name_once() {
        let timeline = [
            message("2026-10-11 18:00", S, I),
            message("2026-10-11 18:25", S, O),
        ];
        let recency = conversation_recency(&timeline, at("2026-10-14T15:00:00Z"));
        let line = greeting_instruction(recency);
        assert!(line.starts_with("GREETING: BY NAME") && line.contains("2 days ago"));
        assert!(line.contains("Use their name only that once"));
    }

    #[test]
    fn a_message_stamped_ahead_of_the_clock_is_just_now() {
        let timeline = [message("2026-10-14 15:05", S, O)];
        assert_eq!(
            conversation_recency(&timeline, at("2026-10-14T15:00:00Z")),
            ConversationRecency::Continuing {
                minutes_since_last: 0
            }
        );
        assert_eq!(describe_ago(0), "just now");
        assert_eq!(describe_ago(90), "about an hour ago");
        assert_eq!(describe_ago(300), "about 5 hours ago");
        assert_eq!(describe_ago(3 * 24 * 60), "3 days ago");
    }
}
