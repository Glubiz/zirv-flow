//! A provider's feedback webhook: the signature scheme its manifest declares, and the reading of
//! one verified body into an event of ours. Pure: the API verifies first and parses second, and
//! writes the event.

use serde_json::Value;
use zirv_flow_core::signature::Scheme;

use super::classify::literal_of;
use crate::manifest::{FeedbackSpec, VerificationSpec};

/// The longest provider event id kept for deduplication, in bytes.
const MAX_EVENT_ID_BYTES: usize = 128;
/// The longest message id read.
const MAX_MESSAGE_ID_CHARS: usize = 64;

/// What the provider reports about a message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FeedbackKind {
    Delivered,
    /// The provider could not deliver. A hard bounce means the address cannot receive mail; a
    /// soft one is temporary, and any bounce kind the manifest does not map is soft.
    Bounced {
        hard: bool,
    },
    Complained,
}

/// One event read from a feedback body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Feedback {
    pub kind: FeedbackKind,
    /// Our message id, as the send placed it and the provider echoed it. Text: the caller parses
    /// it, and an id that is not one of ours is an unknown message.
    pub message_id: String,
    /// The provider's id of the event, when the manifest maps one.
    pub event_id: Option<String>,
}

/// What a verified body came to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FeedbackRead {
    Event(Feedback),
    /// An event type the manifest does not map (an open, a click, a delivery delay): nothing to do.
    Ignored,
    /// No usable type or message id: the body is not one of this provider's events.
    Malformed,
}

/// The signature scheme of a manifest's verification: HMAC-SHA256 over `"{timestamp}.{body}"`
/// when the manifest names a timestamp header, over the body alone otherwise.
pub fn scheme(verification: &VerificationSpec) -> Scheme<'_> {
    Scheme {
        signature_header: &verification.signature_header,
        signature_prefix: &verification.signature_prefix,
        timestamp_header: verification.timestamp_header.as_deref(),
        tolerance_seconds: i64::from(verification.tolerance_seconds),
    }
}

fn text_at(body: &Value, pointer: &str) -> Option<String> {
    body.pointer(pointer).and_then(literal_of)
}

/// Reads one feedback event from `body` (a JSON document whose signature verified).
pub fn read(spec: &FeedbackSpec, body: &Value) -> FeedbackRead {
    let Some(event_type) = text_at(body, &spec.event_type) else {
        return FeedbackRead::Malformed;
    };
    let kind = if spec.delivered.contains(&event_type) {
        FeedbackKind::Delivered
    } else if spec.bounced.contains(&event_type) {
        let hard = spec.bounce_kind.as_ref().is_some_and(|kind| {
            text_at(body, &kind.pointer).is_some_and(|found| kind.hard.contains(&found))
        });
        FeedbackKind::Bounced { hard }
    } else if spec.complained.contains(&event_type) {
        FeedbackKind::Complained
    } else {
        return FeedbackRead::Ignored;
    };
    let message_id = match text_at(body, &spec.message_id) {
        Some(id) if !id.is_empty() && id.chars().count() <= MAX_MESSAGE_ID_CHARS => id,
        _ => return FeedbackRead::Malformed,
    };
    let found = spec
        .event_id
        .as_deref()
        .and_then(|pointer| text_at(body, pointer));
    // An id longer than the store keeps cannot be deduplicated: the event is treated as unmapped.
    if found
        .as_ref()
        .is_some_and(|id| id.len() > MAX_EVENT_ID_BYTES)
    {
        return FeedbackRead::Ignored;
    }
    let event_id = found.filter(|id| !id.is_empty() && !id.chars().any(char::is_control));
    FeedbackRead::Event(Feedback {
        kind,
        message_id,
        event_id,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::manifest::{BounceKindSpec, VerificationSpec};

    fn spec() -> FeedbackSpec {
        FeedbackSpec {
            event_type: "/event".to_owned(),
            delivered: vec!["delivered".to_owned()],
            bounced: vec!["bounce".to_owned()],
            complained: vec!["complaint".to_owned()],
            message_id: "/meta/message_id".to_owned(),
            bounce_kind: Some(BounceKindSpec {
                pointer: "/bounce/kind".to_owned(),
                hard: vec!["permanent".to_owned()],
                soft: vec!["transient".to_owned()],
            }),
            event_id: Some("/id".to_owned()),
            verification: VerificationSpec {
                signature_header: "X-Sig".to_owned(),
                signature_prefix: "sha256=".to_owned(),
                timestamp_header: Some("X-Ts".to_owned()),
                tolerance_seconds: 120,
                secret_field: "hook".to_owned(),
            },
        }
    }

    fn event(kind: &str, extra: Value) -> Value {
        let mut body = json!({"event": kind, "id": "evt-1", "meta": {"message_id": "m-1"}});
        if let (Some(target), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
            target.extend(extra.clone());
        }
        body
    }

    #[test]
    fn each_mapped_event_type_reads_its_kind_message_and_event_id() {
        let read = |body: &Value| read(&spec(), body);
        let feedback = |kind| {
            FeedbackRead::Event(Feedback {
                kind,
                message_id: "m-1".to_owned(),
                event_id: Some("evt-1".to_owned()),
            })
        };

        assert_eq!(
            read(&event("delivered", json!({}))),
            feedback(FeedbackKind::Delivered)
        );
        assert_eq!(
            read(&event("complaint", json!({}))),
            feedback(FeedbackKind::Complained)
        );
        assert_eq!(
            read(&event("bounce", json!({"bounce": {"kind": "permanent"}}))),
            feedback(FeedbackKind::Bounced { hard: true })
        );
        assert_eq!(
            read(&event("bounce", json!({"bounce": {"kind": "transient"}}))),
            feedback(FeedbackKind::Bounced { hard: false })
        );
    }

    #[test]
    fn a_bounce_kind_that_is_unmapped_or_missing_is_soft_and_never_suppresses() {
        let unknown = event("bounce", json!({"bounce": {"kind": "weird"}}));
        let missing = event("bounce", json!({}));

        for body in [unknown, missing] {
            assert!(matches!(
                read(&spec(), &body),
                FeedbackRead::Event(Feedback {
                    kind: FeedbackKind::Bounced { hard: false },
                    ..
                })
            ));
        }
    }

    #[test]
    fn an_unmapped_event_type_is_ignored_and_a_body_without_a_type_or_message_is_malformed() {
        assert_eq!(
            read(&spec(), &event("open", json!({}))),
            FeedbackRead::Ignored
        );
        assert_eq!(read(&spec(), &json!({"id": "x"})), FeedbackRead::Malformed);
        assert_eq!(
            read(&spec(), &json!({"event": "delivered"})),
            FeedbackRead::Malformed
        );
        assert_eq!(
            read(
                &spec(),
                &json!({"event": "delivered", "meta": {"message_id": "x".repeat(65)}})
            ),
            FeedbackRead::Malformed
        );
        assert_eq!(read(&spec(), &json!([1, 2])), FeedbackRead::Malformed);
    }

    #[test]
    fn an_event_id_over_128_bytes_makes_the_event_unmapped() {
        let long = "\u{20ac}".repeat(128);
        let body = json!({"event": "delivered", "id": long, "meta": {"message_id": "m-1"}});
        let fits = json!({"event": "delivered", "id": "\u{20ac}".repeat(42), "meta": {"message_id": "m-1"}});

        assert_eq!(read(&spec(), &body), FeedbackRead::Ignored);
        assert!(matches!(read(&spec(), &fits), FeedbackRead::Event(_)));
    }

    #[test]
    fn the_event_id_is_optional_and_numbers_are_read_as_text() {
        let mut without = spec();
        without.event_id = None;
        let numeric = json!({"event": "delivered", "id": 77, "meta": {"message_id": "m-1"}});

        assert!(matches!(
            read(&without, &numeric),
            FeedbackRead::Event(Feedback { event_id: None, .. })
        ));
        assert!(matches!(
            read(&spec(), &numeric),
            FeedbackRead::Event(Feedback { event_id: Some(ref id), .. }) if id == "77"
        ));
    }

    #[test]
    fn the_scheme_follows_the_manifest_with_and_without_a_timestamp() {
        let mut verification = spec().verification;
        let timestamped = scheme(&verification);
        assert_eq!(timestamped.signature_header, "X-Sig");
        assert_eq!(timestamped.signature_prefix, "sha256=");
        assert_eq!(timestamped.timestamp_header, Some("X-Ts"));
        assert_eq!(timestamped.tolerance_seconds, 120);

        verification.timestamp_header = None;
        assert_eq!(scheme(&verification).timestamp_header, None);
    }
}
