//! Keys and the split hash. The formats are stored and sent to destinations, so they are part of
//! the contract: change one and replays stop deduplicating.

use sha2::{Digest, Sha256};
use uuid::Uuid;

/// The step idempotency key, `{enrollment_id}:{node_id}` (architecture 9.3). Sufficient because
/// graphs are acyclic and re-entry makes a new enrollment.
pub fn step_key(enrollment_id: Uuid, node: &str) -> String {
    format!("{enrollment_id}:{node}")
}

/// The key of the external effect a step produces, `{step_key}:webhook`. One external effect per
/// node today; the suffix names its kind.
pub fn effect_key(enrollment_id: Uuid, node: &str, kind: &str) -> String {
    format!("{}:{kind}", step_key(enrollment_id, node))
}

/// The unique key of the timer job of a wait, `wake:{enrollment_id}:{state_version}`.
pub fn wake_key(enrollment_id: Uuid, state_version: u64) -> String {
    format!("wake:{enrollment_id}:{state_version}")
}

/// The stable hash of `(enrollment_id, node_id)` that `split.random` buckets by: the first eight
/// bytes, big endian, of SHA-256 over the 16 bytes of the enrollment id followed by the UTF-8
/// node id. SHA-256 is specified, so it is the same on every platform and release (std's
/// `DefaultHasher` is neither).
pub fn split_bucket(enrollment_id: Uuid, node: &str) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(enrollment_id.as_bytes());
    hasher.update(node.as_bytes());
    let digest = hasher.finalize();
    let mut head = [0_u8; 8];
    head.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_formats_are_fixed() {
        let id = Uuid::from_u128(1);
        assert_eq!(
            step_key(id, "n1"),
            "00000000-0000-0000-0000-000000000001:n1"
        );
        assert_eq!(
            effect_key(id, "n1", "webhook"),
            "00000000-0000-0000-0000-000000000001:n1:webhook"
        );
        assert_eq!(
            wake_key(id, 7),
            "wake:00000000-0000-0000-0000-000000000001:7"
        );
    }

    #[test]
    fn the_split_hash_is_pinned() {
        // A change here changes which arm every in-flight enrollment takes.
        assert_eq!(split_bucket(Uuid::nil(), "split"), 0x3218_9ed1_907f_6ad0);
        assert_eq!(
            split_bucket(Uuid::nil(), "split"),
            split_bucket(Uuid::nil(), "split")
        );
        assert_ne!(
            split_bucket(Uuid::nil(), "a"),
            split_bucket(Uuid::nil(), "b")
        );
        assert_ne!(
            split_bucket(Uuid::from_u128(1), "a"),
            split_bucket(Uuid::from_u128(2), "a")
        );
    }
}
