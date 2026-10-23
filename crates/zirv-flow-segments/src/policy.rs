//! What a consent policy rule reads beyond the person: the message being sent and the messages
//! already sent (architecture 10.1). A segment has neither, so the leaves that need them are
//! valid in a policy only. The caller loads the facts; nothing here touches a database.

use time::OffsetDateTime;
use zirv_flow_core::consent::{Channel, Dimensions, RecordedConsent};

/// The send a policy is evaluated for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SendContext {
    pub channel: Channel,
    /// The message's dimension values, the topic not among them.
    pub dimensions: Dimensions,
    /// What the person's records say about this message.
    pub consent: RecordedConsent,
}

/// One message that was sent to the person.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotMessage {
    pub channel: Channel,
    pub dimensions: Dimensions,
    /// When the message ended as `sent`.
    pub sent_at: OffsetDateTime,
}

/// The facts the evaluator reads for a policy rule, beside the person's snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyFacts {
    pub send: SendContext,
    /// The person's sent messages, test sends excluded.
    pub messages: Vec<SnapshotMessage>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_facts_hold_the_send_and_the_messages() {
        let facts = PolicyFacts {
            send: SendContext {
                channel: Channel::Email,
                dimensions: Dimensions::new(),
                consent: RecordedConsent::None,
            },
            messages: Vec::new(),
        };
        assert_eq!(facts.send.consent, RecordedConsent::None);
        assert!(facts.messages.is_empty());
    }
}
