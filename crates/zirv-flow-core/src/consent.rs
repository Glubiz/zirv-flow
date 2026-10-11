//! The pure consent rules (architecture sections 6, 7.3 and 10.1): channels, consent states,
//! topic policies, the send-time `allowed` check and the merge of two people's states.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use time::OffsetDateTime;

use crate::ValidationError;

const MAX_TOPIC_KEY_CHARS: usize = 64;

/// A channel consent is held for. Webhooks need no consent; SMS and push come later.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Channel {
    Email,
}

impl Channel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Email => "email",
        }
    }
}

impl FromStr for Channel {
    type Err = ValidationError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "email" => Ok(Self::Email),
            _ => Err(ValidationError::new("channel", "must be email")),
        }
    }
}

impl fmt::Display for Channel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What a person has said about a channel or a topic.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ConsentStatus {
    Subscribed,
    Unsubscribed,
    PendingConfirmation,
    /// Nothing is known. Never written: it is what an absent row means.
    Unknown,
}

impl ConsentStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Subscribed => "subscribed",
            Self::Unsubscribed => "unsubscribed",
            Self::PendingConfirmation => "pending_confirmation",
            Self::Unknown => "unknown",
        }
    }

    /// Whether the status records a decision by the person: only `subscribed` and
    /// `unsubscribed` do.
    pub const fn is_explicit(self) -> bool {
        matches!(self, Self::Subscribed | Self::Unsubscribed)
    }
}

impl FromStr for ConsentStatus {
    type Err = ValidationError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "subscribed" => Ok(Self::Subscribed),
            "unsubscribed" => Ok(Self::Unsubscribed),
            "pending_confirmation" => Ok(Self::PendingConfirmation),
            "unknown" => Ok(Self::Unknown),
            _ => Err(ValidationError::new(
                "status",
                "must be subscribed, unsubscribed, pending_confirmation or unknown",
            )),
        }
    }
}

impl fmt::Display for ConsentStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// How a topic treats a person who never answered.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TopicPolicy {
    /// Allowed only when the person is `subscribed`.
    OptIn,
    /// Allowed unless the person is `unsubscribed`.
    OptOut,
}

impl TopicPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OptIn => "opt_in",
            Self::OptOut => "opt_out",
        }
    }
}

impl FromStr for TopicPolicy {
    type Err = ValidationError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "opt_in" => Ok(Self::OptIn),
            "opt_out" => Ok(Self::OptOut),
            _ => Err(ValidationError::new("policy", "must be opt_in or opt_out")),
        }
    }
}

impl fmt::Display for TopicPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Where a consent change came from.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ChangeSource {
    Api,
    Ui,
    Import,
    Merge,
    UnsubscribeLink,
    Ingest,
}

impl ChangeSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Ui => "ui",
            Self::Import => "import",
            Self::Merge => "merge",
            Self::UnsubscribeLink => "unsubscribe_link",
            Self::Ingest => "ingest",
        }
    }
}

impl FromStr for ChangeSource {
    type Err = ValidationError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        [
            Self::Api,
            Self::Ui,
            Self::Import,
            Self::Merge,
            Self::UnsubscribeLink,
            Self::Ingest,
        ]
        .into_iter()
        .find(|source| source.as_str() == text)
        .ok_or(ValidationError::new(
            "source",
            "is not a known change source",
        ))
    }
}

/// Why an identifier is suppressed.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SuppressionReason {
    HardBounce,
    Complaint,
    Manual,
    /// The person was erased. Permanent: it can never be removed.
    Erased,
}

impl SuppressionReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HardBounce => "hard_bounce",
            Self::Complaint => "complaint",
            Self::Manual => "manual",
            Self::Erased => "erased",
        }
    }
}

impl FromStr for SuppressionReason {
    type Err = ValidationError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "hard_bounce" => Ok(Self::HardBounce),
            "complaint" => Ok(Self::Complaint),
            "manual" => Ok(Self::Manual),
            "erased" => Ok(Self::Erased),
            _ => Err(ValidationError::new(
                "reason",
                "must be hard_bounce, complaint, manual or erased",
            )),
        }
    }
}

/// A consent status and when it was set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct State {
    pub status: ConsentStatus,
    pub changed_at: OffsetDateTime,
}

/// Whether a send on a channel is permitted for a topic. A channel-wide `unsubscribed` blocks
/// every topic; otherwise the topic's own state is judged under its policy. A state nobody
/// recorded is [`ConsentStatus::Unknown`].
pub const fn allowed(
    channel_wide: ConsentStatus,
    topic: ConsentStatus,
    policy: TopicPolicy,
) -> bool {
    if matches!(channel_wide, ConsentStatus::Unsubscribed) {
        return false;
    }
    match policy {
        TopicPolicy::OptIn => matches!(topic, ConsentStatus::Subscribed),
        TopicPolicy::OptOut => !matches!(topic, ConsentStatus::Unsubscribed),
    }
}

/// Combines the states two people hold for one channel and topic when they become one.
///
/// The most recent explicit decision wins, and `unsubscribed` wins a tie at the same instant.
/// An explicit decision beats any state that is not explicit. When neither side is explicit
/// the more recent state wins, and `pending_confirmation` wins a tie over `unknown`.
pub fn merge(a: Option<State>, b: Option<State>) -> Option<State> {
    let (a, b) = match (a, b) {
        (None, other) | (other, None) => return other,
        (Some(a), Some(b)) => (a, b),
    };
    Some(match (a.status.is_explicit(), b.status.is_explicit()) {
        (true, false) => a,
        (false, true) => b,
        (true, true) => later(a, b, ConsentStatus::Unsubscribed),
        (false, false) => later(a, b, ConsentStatus::PendingConfirmation),
    })
}

/// The more recent of the two; at the same instant, the one with the `preferred` status,
/// else `a`.
fn later(a: State, b: State, preferred: ConsentStatus) -> State {
    match a.changed_at.cmp(&b.changed_at) {
        Ordering::Greater => a,
        Ordering::Less => b,
        Ordering::Equal if b.status == preferred && a.status != preferred => b,
        Ordering::Equal => a,
    }
}

/// Checks a topic key: lowercase letters, digits, `_` and `-`, 1 to 64 characters.
pub fn validate_topic_key(key: &str) -> Result<(), ValidationError> {
    let valid = !key.is_empty()
        && key.len() <= MAX_TOPIC_KEY_CHARS
        && key.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        });
    if valid {
        Ok(())
    } else {
        Err(ValidationError::new(
            "key",
            "must be 1 to 64 lowercase letters, digits, underscores or hyphens",
        ))
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::ConsentStatus::{PendingConfirmation, Subscribed, Unknown, Unsubscribed};
    use super::TopicPolicy::{OptIn, OptOut};
    use super::*;

    const T1: OffsetDateTime = datetime!(2026-01-01 00:00 UTC);
    const T2: OffsetDateTime = datetime!(2026-01-02 00:00 UTC);

    fn state(status: ConsentStatus, changed_at: OffsetDateTime) -> Option<State> {
        Some(State { status, changed_at })
    }

    #[test]
    fn opt_in_topics_need_a_subscription_and_opt_out_topics_only_lack_an_unsubscribe() {
        for (topic, opt_in, opt_out) in [
            (Subscribed, true, true),
            (Unsubscribed, false, false),
            (PendingConfirmation, false, true),
            (Unknown, false, true),
        ] {
            assert_eq!(allowed(Unknown, topic, OptIn), opt_in, "{topic} opt_in");
            assert_eq!(allowed(Unknown, topic, OptOut), opt_out, "{topic} opt_out");
        }
    }

    #[test]
    fn a_channel_wide_unsubscribe_blocks_every_topic() {
        for topic in [Subscribed, Unsubscribed, PendingConfirmation, Unknown] {
            for policy in [OptIn, OptOut] {
                assert!(!allowed(Unsubscribed, topic, policy), "{topic} {policy}");
            }
        }
        assert!(allowed(Subscribed, Subscribed, OptIn));
        assert!(allowed(PendingConfirmation, Unknown, OptOut));
    }

    #[test]
    fn the_more_recent_explicit_decision_wins_in_either_order() {
        let old = state(Unsubscribed, T1);
        let new = state(Subscribed, T2);

        assert_eq!(merge(old, new), new);
        assert_eq!(merge(new, old), new);
    }

    #[test]
    fn unsubscribed_wins_a_tie_in_either_order() {
        let yes = state(Subscribed, T1);
        let no = state(Unsubscribed, T1);

        assert_eq!(merge(yes, no), no);
        assert_eq!(merge(no, yes), no);
    }

    #[test]
    fn a_missing_side_yields_the_other() {
        let one = state(Subscribed, T1);

        assert_eq!(merge(one, None), one);
        assert_eq!(merge(None, one), one);
        assert_eq!(merge(None, None), None);
    }

    #[test]
    fn an_explicit_decision_beats_a_newer_state_that_is_not_explicit() {
        let decision = state(Unsubscribed, T1);
        let pending = state(PendingConfirmation, T2);

        assert_eq!(merge(decision, pending), decision);
        assert_eq!(merge(pending, decision), decision);
    }

    #[test]
    fn without_a_decision_the_more_recent_state_wins() {
        let old = state(Unknown, T1);
        let new = state(PendingConfirmation, T2);

        assert_eq!(merge(old, new), new);
        assert_eq!(merge(new, old), new);
        assert_eq!(
            merge(state(Unknown, T1), state(PendingConfirmation, T1)),
            state(PendingConfirmation, T1)
        );
        assert_eq!(
            merge(state(PendingConfirmation, T1), state(Unknown, T1)),
            state(PendingConfirmation, T1)
        );
    }

    #[test]
    fn only_subscribed_and_unsubscribed_are_explicit() {
        assert!(Subscribed.is_explicit() && Unsubscribed.is_explicit());
        assert!(!PendingConfirmation.is_explicit() && !Unknown.is_explicit());
    }

    #[test]
    fn names_round_trip_and_unknown_names_are_refused() {
        for status in [Subscribed, Unsubscribed, PendingConfirmation, Unknown] {
            assert_eq!(status.as_str().parse::<ConsentStatus>().unwrap(), status);
        }
        for policy in [OptIn, OptOut] {
            assert_eq!(policy.as_str().parse::<TopicPolicy>().unwrap(), policy);
        }
        assert_eq!("email".parse::<Channel>().unwrap(), Channel::Email);
        assert!("sms".parse::<Channel>().is_err());
        assert!("maybe".parse::<ConsentStatus>().is_err());
        assert!("opt_sideways".parse::<TopicPolicy>().is_err());
    }

    #[test]
    fn sources_and_reasons_round_trip_and_unknown_names_are_refused() {
        for source in ["api", "ui", "import", "merge", "unsubscribe_link", "ingest"] {
            assert_eq!(source.parse::<ChangeSource>().unwrap().as_str(), source);
        }
        for reason in ["hard_bounce", "complaint", "manual", "erased"] {
            assert_eq!(
                reason.parse::<SuppressionReason>().unwrap().as_str(),
                reason
            );
        }
        assert!("carrier_pigeon".parse::<ChangeSource>().is_err());
        assert!("rude".parse::<SuppressionReason>().is_err());
    }

    #[test]
    fn a_topic_key_is_lowercase_letters_digits_underscores_and_hyphens() {
        for good in ["news", "a-b_c9", &"x".repeat(64)] {
            assert!(validate_topic_key(good).is_ok(), "{good}");
        }
        for bad in ["", "News", "a b", "a.b", "\u{e9}", &"x".repeat(65)] {
            assert!(validate_topic_key(bad).is_err(), "{bad}");
        }
    }
}
