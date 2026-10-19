//! The signed call of an `action.webhook` node (architecture 9.1, ruling 9cb0bea9): the fixed
//! JSON envelope, the header names and the signature value. Pure: the job that sends it lives in
//! `zirv-flow-jobs`.
//!
//! The headers are named and formatted like the inbound scheme's defaults, so a receiver that
//! verifies a zirv-flow inbound hook verifies this with the same code: the signature header holds
//! `sha256=` and 64 hex digits of the HMAC over `"{timestamp}.{body}"`, and the timestamp header
//! holds the decimal unix seconds that were signed.

use serde::Serialize;
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::TraceId;
use crate::signature::{
    DEFAULT_SIGNATURE_HEADER, DEFAULT_SIGNATURE_PREFIX, DEFAULT_TIMESTAMP_HEADER, sign,
};

/// The header holding the signature.
pub const SIGNATURE_HEADER: &str = DEFAULT_SIGNATURE_HEADER;
/// The header holding the unix seconds that were signed.
pub const TIMESTAMP_HEADER: &str = DEFAULT_TIMESTAMP_HEADER;
/// The header holding the idempotency key, which is also the key in the body. A receiver
/// that sees it twice has seen the same effect twice.
pub const IDEMPOTENCY_HEADER: &str = "X-Zirv-Idempotency-Key";

/// How long the previous signing secret stays valid after a rotation: 24 hours. During the
/// window every call carries a signature made with the new secret and one made with the previous
/// secret, so a receiver can switch to the new secret at any time inside it.
pub const SIGNING_GRACE_SECONDS: i64 = 24 * 60 * 60;

/// The body of a webhook call. The fields are fixed; only `traits` varies, and only in the keys
/// the node lists.
#[derive(Clone, Debug, Serialize)]
pub struct Envelope<'a> {
    pub flow_id: Uuid,
    pub version: i32,
    pub node_id: &'a str,
    pub enrollment_id: Uuid,
    pub person_id: Uuid,
    /// The enrollment's trace, in the form of the `x-trace-id` header: 32 lowercase hex digits.
    pub trace_id: TraceId,
    /// The effect key: the same for every attempt and every redelivery of this effect.
    pub idempotency_key: &'a str,
    /// When the step that produced the call ran, in RFC 3339.
    pub occurred_at: &'a str,
    /// The node's listed trait keys at the person's effective values when the call was built;
    /// a key the person has no value for is `null`.
    pub traits: Map<String, Value>,
}

impl Envelope<'_> {
    /// The exact bytes that are signed and sent.
    pub fn body(&self) -> Vec<u8> {
        // Serializing a struct of strings, numbers and a JSON map cannot fail.
        serde_json::to_vec(self).expect("an envelope serializes")
    }
}

/// The value of the signature header for `body` signed at `timestamp` (unix seconds).
pub fn signature_value(secret: &[u8], timestamp: i64, body: &[u8]) -> String {
    format!(
        "{DEFAULT_SIGNATURE_PREFIX}{}",
        sign(secret, Some(timestamp), body)
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::signature::{Scheme, verify};

    fn envelope(traits: Map<String, Value>) -> Envelope<'static> {
        Envelope {
            flow_id: Uuid::from_u128(1),
            version: 3,
            node_id: "hook",
            enrollment_id: Uuid::from_u128(2),
            person_id: Uuid::from_u128(3),
            trace_id: TraceId::from_uuid(Uuid::from_u128(4)),
            idempotency_key: "key",
            occurred_at: "2026-01-02T03:04:05Z",
            traits,
        }
    }

    #[test]
    fn the_envelope_has_the_fixed_fields_and_the_listed_traits() {
        let mut traits = Map::new();
        traits.insert("plan".into(), json!("pro"));
        traits.insert("missing".into(), Value::Null);

        let body: Value = serde_json::from_slice(&envelope(traits).body()).unwrap();

        assert_eq!(
            body,
            json!({
                "flow_id": "00000000-0000-0000-0000-000000000001",
                "version": 3,
                "node_id": "hook",
                "enrollment_id": "00000000-0000-0000-0000-000000000002",
                "person_id": "00000000-0000-0000-0000-000000000003",
                "trace_id": "00000000000000000000000000000004",
                "idempotency_key": "key",
                "occurred_at": "2026-01-02T03:04:05Z",
                "traits": {"plan": "pro", "missing": null},
            })
        );
    }

    #[test]
    fn the_signature_verifies_with_the_inbound_scheme_over_the_exact_bytes() {
        let body = envelope(Map::new()).body();
        let header = signature_value(b"zfwh_secret", 1_700_000_000, &body);
        let scheme = Scheme {
            signature_header: SIGNATURE_HEADER,
            signature_prefix: "sha256=",
            timestamp_header: Some(TIMESTAMP_HEADER),
            tolerance_seconds: 300,
        };
        let lookup = |name: &str| match name {
            SIGNATURE_HEADER => Some(header.as_str()),
            TIMESTAMP_HEADER => Some("1700000000"),
            _ => None,
        };

        assert!(verify(&scheme, b"zfwh_secret", &lookup, &body, 1_700_000_100).is_ok());
        assert!(verify(&scheme, b"other", &lookup, &body, 1_700_000_100).is_err());
        let mut changed = body.clone();
        changed.push(b' ');
        assert!(verify(&scheme, b"zfwh_secret", &lookup, &changed, 1_700_000_100).is_err());
    }

    #[test]
    fn the_header_names_are_the_inbound_defaults_and_the_grace_is_a_day() {
        assert_eq!(SIGNATURE_HEADER, "X-Zirv-Signature");
        assert_eq!(TIMESTAMP_HEADER, "X-Zirv-Timestamp");
        assert_eq!(IDEMPOTENCY_HEADER, "X-Zirv-Idempotency-Key");
        assert_eq!(SIGNING_GRACE_SECONDS, 86_400);
    }
}
