//! The wire formats of credentials: API keys and CSRF tokens.

use uuid::Uuid;
use zirv_flow_core::ApiKeyId;
use zirv_flow_core::token::{random_token, sha256};

const KEY_PREFIX: &str = "zfk_";
/// Length of a UUID in simple (hyphen-less) form.
const ID_LENGTH: usize = 32;
const SECRET_BYTES: usize = 32;

/// A freshly generated key: the plaintext to show once and the hash to store.
pub(crate) struct NewKey {
    pub(crate) plaintext: String,
    pub(crate) secret_hash: [u8; 32],
}

/// Generates `zfk_<id>_<secret>` with a 32-byte random secret.
pub(crate) fn new_key(id: ApiKeyId) -> NewKey {
    let secret = random_token(SECRET_BYTES);
    NewKey {
        plaintext: format!("{KEY_PREFIX}{}_{secret}", id.as_uuid().simple()),
        secret_hash: sha256(&secret),
    }
}

/// Splits a presented key into its id and secret. Anything malformed is `None`.
pub(crate) fn parse_key(key: &str) -> Option<(ApiKeyId, &str)> {
    let rest = key.strip_prefix(KEY_PREFIX)?;
    let id = rest.get(..ID_LENGTH)?;
    let secret = rest.get(ID_LENGTH..)?.strip_prefix('_')?;
    // The secret is base64url; reject empty or oversized input before hashing it.
    let plausible = !secret.is_empty()
        && secret.len() <= 128
        && secret
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !plausible || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let id = Uuid::try_parse(id).ok()?;
    Some((ApiKeyId::from_uuid(id), secret))
}

/// The CSRF token that belongs to a session token.
///
/// Only the hash of the session token is stored, so a stored random CSRF token could not
/// be handed out a second time (by `GET /session`). Deriving it from the session token
/// with a domain-separation prefix makes it reproducible for the holder of the cookie,
/// while a database leak (which holds only hashes) yields neither.
pub(crate) fn csrf_token(session_token: &str) -> String {
    let digest = sha256(&format!("zirv-flow csrf v1:{session_token}"));
    let mut bytes = String::with_capacity(64);
    for byte in digest {
        bytes.push_str(&format!("{byte:02x}"));
    }
    bytes
}

#[cfg(test)]
mod tests {
    use zirv_flow_core::token::hash_matches;

    use super::*;

    #[test]
    fn a_generated_key_parses_back_to_its_id_and_secret_hash() {
        let id = ApiKeyId::new();

        let key = new_key(id);
        let (parsed, secret) = parse_key(&key.plaintext).unwrap();

        assert_eq!(parsed, id);
        assert!(key.plaintext.starts_with("zfk_"));
        assert!(hash_matches(secret, &key.secret_hash));
        assert_eq!(secret.len(), 43);
    }

    #[test]
    fn the_secret_may_contain_underscores_and_dashes() {
        let id = ApiKeyId::new();
        let key = format!("zfk_{}_a_b-c", id.as_uuid().simple());

        assert_eq!(parse_key(&key).unwrap().1, "a_b-c");
    }

    #[test]
    fn malformed_keys_are_rejected() {
        let id = ApiKeyId::new().as_uuid().simple().to_string();
        for bad in [
            "".to_owned(),
            "zfk_".to_owned(),
            format!("zfk_{id}"),
            format!("zfk_{id}_"),
            format!("zfk_{id}x"),
            format!("zfk_{id}_se cret"),
            format!("zfk_{id}_{}", "a".repeat(129)),
            format!("xxx_{id}_secret"),
            format!("zfk_{}_secret", "g".repeat(32)),
            format!("zfk_é{}_secret", &id[..30]),
        ] {
            assert!(parse_key(&bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn the_csrf_token_is_stable_per_session_and_differs_between_sessions() {
        let first = csrf_token("session-one");

        assert_eq!(first, csrf_token("session-one"));
        assert_ne!(first, csrf_token("session-two"));
        assert_eq!(first.len(), 64);
    }

    #[test]
    fn the_csrf_token_is_unrelated_to_the_stored_session_hash() {
        let hex_of_session_hash: String = sha256("session-one")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();

        assert_ne!(csrf_token("session-one"), hex_of_session_hash);
    }
}
