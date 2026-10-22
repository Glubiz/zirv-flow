//! The pure consent rules (architecture sections 6, 7.3 and 10.1): channels, consent states,
//! topic policies, the send-time `allowed` check and the merge of two people's states.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use time::OffsetDateTime;
use uuid::Uuid;

use crate::ValidationError;

const MAX_TOPIC_KEY_CHARS: usize = 64;

/// The built-in dimension whose values are the workspace's topics. A scope holds its topic
/// under this key; the store resolves it from the `topics` rows, so nothing is copied.
pub const TOPIC_DIMENSION: &str = "topic";

/// A consent scope or a message's coordinates: dimension key to value key. An omitted
/// dimension means all values. Sorted by key, so two equal maps iterate alike.
pub type Dimensions = BTreeMap<String, String>;

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
///
/// This is the two-state check of one channel and one topic. Sends read consent with
/// [`read`], which also handles dimension-scoped records and lets the latest change win.
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

/// The canonical text of a dimension map: `key=value` pairs sorted bytewise by key and
/// joined by `;`, empty for the empty map. Keys and values are `[a-z0-9_-]` only, so the
/// form is unambiguous. The store's `consent_dimensions_canonical` SQL function produces
/// the same text for the generated unique-key column.
pub fn canonical_dimensions(dimensions: &Dimensions) -> String {
    let mut text = String::new();
    for (key, value) in dimensions {
        if !text.is_empty() {
            text.push(';');
        }
        text.push_str(key);
        text.push('=');
        text.push_str(value);
    }
    text
}

/// Whether a record scoped to `scope` covers a message with the coordinates `message`: every
/// dimension the scope names must carry the same value in the message. An empty scope covers
/// every message.
pub fn covers(scope: &Dimensions, message: &Dimensions) -> bool {
    scope
        .iter()
        .all(|(key, value)| message.get(key) == Some(value))
}

/// One consent record of a person for one channel: the scope it covers, its state, and the id
/// of the change that last set it (the tie-break between equal times).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Record {
    pub scope: Dimensions,
    pub state: State,
    pub change_id: Uuid,
}

impl Record {
    /// What orders two records: when the change happened, then the change id.
    fn order(&self) -> (OffsetDateTime, Uuid) {
        (self.state.changed_at, self.change_id)
    }
}

/// What [`read`] decided.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Read {
    pub allowed: bool,
    /// The change that decided it; `None` when no record covers the message and the topic
    /// policy's default applies.
    pub deciding_change: Option<Uuid>,
}

/// The read rule (OQ-32, architecture 10.1): whether a send whose message has the coordinates
/// `message` is permitted, given the person's records for the channel. Among the records
/// that cover the message the latest change wins, ordered by (`changed_at`, change id). A
/// winner of `subscribed` allows and `unsubscribed` blocks; any other status, and no covering
/// record at all, falls back to the topic policy's default: an opt-in topic blocks and an
/// opt-out topic allows.
pub fn read(message: &Dimensions, records: &[Record], policy: TopicPolicy) -> Read {
    let winner = records
        .iter()
        .filter(|record| covers(&record.scope, message))
        .max_by_key(|record| record.order());
    let default = matches!(policy, TopicPolicy::OptOut);
    match winner {
        None => Read {
            allowed: default,
            deciding_change: None,
        },
        Some(record) => Read {
            allowed: match record.state.status {
                ConsentStatus::Subscribed => true,
                ConsentStatus::Unsubscribed => false,
                ConsentStatus::PendingConfirmation | ConsentStatus::Unknown => default,
            },
            deciding_change: Some(record.change_id),
        },
    }
}

/// Combines the records two people hold when they become one. Records of the same scope are
/// resolved by [`merge`]; at the same instant and status the later change id is kept. A scope
/// only one side holds is kept as it is. The result is ordered by canonical scope.
pub fn merge_records(a: Vec<Record>, b: Vec<Record>) -> Vec<Record> {
    let mut merged: BTreeMap<String, Record> = BTreeMap::new();
    for record in a.into_iter().chain(b) {
        let slot = canonical_dimensions(&record.scope);
        match merged.remove(&slot) {
            None => {
                merged.insert(slot, record);
            }
            Some(held) => {
                let winner = merge(Some(held.state), Some(record.state));
                let keep = match winner {
                    Some(state) if state == held.state && state == record.state => {
                        if record.order() > held.order() {
                            record
                        } else {
                            held
                        }
                    }
                    Some(state) if state == held.state => held,
                    _ => record,
                };
                merged.insert(slot, keep);
            }
        }
    }
    merged.into_values().collect()
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

/// Checks a dimension key: the topic key charset, and not the reserved `topic`.
pub fn validate_dimension_key(key: &str) -> Result<(), ValidationError> {
    validate_topic_key(key)?;
    if key == TOPIC_DIMENSION {
        return Err(ValidationError::new("key", "topic is a built-in dimension"));
    }
    Ok(())
}

/// Checks a dimension value key: the topic key charset.
pub fn validate_dimension_value_key(key: &str) -> Result<(), ValidationError> {
    validate_topic_key(key)
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

    fn dims(pairs: &[(&str, &str)]) -> Dimensions {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn record(
        scope: &[(&str, &str)],
        status: ConsentStatus,
        at: OffsetDateTime,
        change: u128,
    ) -> Record {
        Record {
            scope: dims(scope),
            state: State {
                status,
                changed_at: at,
            },
            change_id: id(change),
        }
    }

    #[test]
    fn the_latest_change_among_the_covering_records_wins() {
        let message = dims(&[("topic", "news"), ("vertical", "retail")]);
        let records = [
            record(&[], Unsubscribed, T1, 1),
            record(&[("topic", "news")], Subscribed, T2, 2),
        ];

        let verdict = read(&message, &records, OptIn);

        assert_eq!(
            verdict,
            Read {
                allowed: true,
                deciding_change: Some(id(2))
            }
        );
        let older = [
            record(&[], Subscribed, T1, 1),
            record(&[("topic", "news")], Unsubscribed, T2, 2),
        ];
        assert!(!read(&message, &older, OptOut).allowed);
    }

    #[test]
    fn equal_timestamps_are_ordered_by_change_id_whatever_the_input_order() {
        let message = dims(&[("vertical", "retail")]);
        let mut records = vec![
            record(&[], Subscribed, T1, 9),
            record(&[("vertical", "retail")], Unsubscribed, T1, 5),
        ];

        let forward = read(&message, &records, OptOut);
        records.reverse();
        let backward = read(&message, &records, OptOut);

        assert_eq!(forward, backward);
        assert_eq!(forward.deciding_change, Some(id(9)));
        assert!(forward.allowed);
        records[0].change_id = id(10);
        let flipped = read(&message, &records, OptOut);
        assert_eq!(flipped.deciding_change, Some(id(10)));
        assert!(!flipped.allowed);
    }

    #[test]
    fn no_covering_record_means_the_topic_policy_default() {
        let message = dims(&[("vertical", "retail")]);
        let elsewhere = [record(&[("vertical", "b2b")], Unsubscribed, T1, 1)];

        for records in [&[][..], &elsewhere[..]] {
            let opt_in = read(&message, records, OptIn);
            let opt_out = read(&message, records, OptOut);

            assert_eq!((opt_in.allowed, opt_in.deciding_change), (false, None));
            assert_eq!((opt_out.allowed, opt_out.deciding_change), (true, None));
        }
    }

    #[test]
    fn a_status_that_is_not_a_decision_falls_back_to_the_default() {
        let message = dims(&[]);
        for status in [PendingConfirmation, Unknown] {
            let records = [record(&[], status, T1, 1)];

            assert!(!read(&message, &records, OptIn).allowed, "{status}");
            assert!(read(&message, &records, OptOut).allowed, "{status}");
            assert_eq!(read(&message, &records, OptIn).deciding_change, Some(id(1)));
        }
    }

    #[test]
    fn a_vertical_scoped_unsubscribe_blocks_only_that_vertical() {
        let records = [
            record(&[("topic", "news")], Subscribed, T1, 1),
            record(
                &[("topic", "news"), ("vertical", "retail")],
                Unsubscribed,
                T2,
                2,
            ),
        ];
        let retail = dims(&[("topic", "news"), ("vertical", "retail")]);
        let b2b = dims(&[("topic", "news"), ("vertical", "b2b")]);
        let unscoped = dims(&[("topic", "news")]);

        assert!(!read(&retail, &records, OptIn).allowed);
        assert!(read(&b2b, &records, OptIn).allowed);
        assert!(read(&unscoped, &records, OptIn).allowed);
        assert!(read(&b2b, &records, OptOut).allowed);
    }

    #[test]
    fn an_all_values_unsubscribe_blocks_every_value() {
        let records = [
            record(&[("vertical", "retail")], Subscribed, T1, 1),
            record(&[], Unsubscribed, T2, 2),
        ];

        for vertical in ["retail", "b2b", "other"] {
            let message = dims(&[("topic", "news"), ("vertical", vertical)]);
            assert!(!read(&message, &records, OptOut).allowed, "{vertical}");
        }
        assert!(!read(&dims(&[]), &records, OptOut).allowed);
    }

    #[test]
    fn a_scope_covers_a_message_that_carries_all_its_values() {
        let scope = dims(&[("vertical", "retail"), ("topic", "news")]);

        assert!(covers(
            &scope,
            &dims(&[("topic", "news"), ("vertical", "retail"), ("x", "y")])
        ));
        assert!(!covers(&scope, &dims(&[("topic", "news")])));
        assert!(!covers(
            &scope,
            &dims(&[("topic", "news"), ("vertical", "b2b")])
        ));
        assert!(covers(&dims(&[]), &dims(&[])));
    }

    #[test]
    fn equal_maps_have_one_canonical_text_sorted_by_key() {
        let a = dims(&[("vertical", "retail"), ("region", "eu")]);
        let b = dims(&[("region", "eu"), ("vertical", "retail")]);

        assert_eq!(canonical_dimensions(&a), "region=eu;vertical=retail");
        assert_eq!(canonical_dimensions(&a), canonical_dimensions(&b));
        assert_eq!(canonical_dimensions(&dims(&[])), "");
        assert_ne!(
            canonical_dimensions(&dims(&[("a", "b-c")])),
            canonical_dimensions(&dims(&[("a-b", "c")]))
        );
    }

    #[test]
    fn merging_records_keeps_the_latest_per_scope_and_both_sides_own_scopes() {
        let a = vec![
            record(&[], Unsubscribed, T1, 1),
            record(&[("vertical", "retail")], Subscribed, T1, 2),
        ];
        let b = vec![
            record(&[], Subscribed, T2, 3),
            record(&[("vertical", "b2b")], Unsubscribed, T1, 4),
        ];

        let merged = merge_records(a.clone(), b.clone());

        let shown: Vec<(String, ConsentStatus, u128)> = merged
            .iter()
            .map(|r| {
                (
                    canonical_dimensions(&r.scope),
                    r.state.status,
                    r.change_id.as_u128(),
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                (String::new(), Subscribed, 3),
                ("vertical=b2b".to_owned(), Unsubscribed, 4),
                ("vertical=retail".to_owned(), Subscribed, 2),
            ]
        );
        assert_eq!(merge_records(b, a), merged);
    }

    #[test]
    fn merging_records_of_one_instant_prefers_unsubscribed_then_the_later_change_id() {
        let yes = record(&[("vertical", "retail")], Subscribed, T1, 8);
        let no = record(&[("vertical", "retail")], Unsubscribed, T1, 3);
        let later_yes = record(&[("vertical", "retail")], Subscribed, T1, 9);

        let against = merge_records(vec![yes.clone()], vec![no.clone()]);
        let reversed = merge_records(vec![no.clone()], vec![yes.clone()]);
        assert_eq!((against, reversed), (vec![no.clone()], vec![no]));
        assert_eq!(
            merge_records(vec![yes], vec![later_yes.clone()]),
            [later_yes]
        );
        assert!(merge_records(vec![], vec![]).is_empty());
    }

    #[test]
    fn a_dimension_key_is_a_topic_key_that_is_not_the_reserved_topic() {
        assert!(validate_dimension_key("vertical").is_ok());
        assert!(validate_dimension_key("topic").is_err());
        assert!(validate_dimension_key("Vertical").is_err());
        assert!(validate_dimension_value_key("retail-eu").is_ok());
        assert!(validate_dimension_value_key("topic").is_ok());
        assert!(validate_dimension_value_key("a b").is_err());
    }
}
