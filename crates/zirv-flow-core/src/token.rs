//! Random tokens, hashing and constant-time comparison for sessions, CSRF and API keys.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// `bytes` random bytes from the operating system, as base64url without padding.
///
/// # Panics
///
/// If the operating system has no randomness source. Continuing without one would
/// mean issuing guessable credentials.
pub fn random_token(bytes: usize) -> String {
    let mut buffer = vec![0_u8; bytes];
    getrandom::fill(&mut buffer).expect("the operating system provides randomness");
    URL_SAFE_NO_PAD.encode(buffer)
}

/// The SHA-256 digest of a token. Tokens carry 256 bits of entropy, so a fast hash is enough.
pub fn sha256(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

/// The SHA-256 digest of bytes.
pub fn sha256_bytes(value: &[u8]) -> [u8; 32] {
    Sha256::digest(value).into()
}

/// A SHA-256 digest built from pieces, for a body that is streamed.
#[derive(Default)]
pub struct Sha256Stream(Sha256);

impl Sha256Stream {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, piece: &[u8]) {
        self.0.update(piece);
    }

    pub fn finish(self) -> [u8; 32] {
        self.0.finalize().into()
    }
}

/// Compares two byte strings in constant time (for equal lengths).
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.ct_eq(right).into()
}

/// Whether `value` hashes to `expected`, compared in constant time.
pub fn hash_matches(value: &str, expected: &[u8]) -> bool {
    constant_time_eq(&sha256(value), expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_base64url_without_padding_of_the_requested_size() {
        let token = random_token(32);

        assert_eq!(token.len(), 43);
        assert!(
            token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
        assert_eq!(URL_SAFE_NO_PAD.decode(&token).unwrap().len(), 32);
    }

    #[test]
    fn tokens_are_not_repeated() {
        assert_ne!(random_token(32), random_token(32));
    }

    #[test]
    fn a_streamed_digest_equals_the_one_shot_digest() {
        let mut stream = Sha256Stream::new();
        stream.update(b"a");
        stream.update(b"bc");

        assert_eq!(stream.finish(), sha256("abc"));
        assert_eq!(sha256_bytes(b"abc"), sha256("abc"));
    }

    #[test]
    fn sha256_matches_the_known_vector() {
        let digest = sha256("abc");

        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn comparison_accepts_equal_and_rejects_different_or_short_inputs() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
    }

    #[test]
    fn a_hash_matches_only_its_own_value() {
        let hash = sha256("secret");

        assert!(hash_matches("secret", &hash));
        assert!(!hash_matches("secret2", &hash));
        assert!(!hash_matches("secret", &hash[..16]));
    }
}
