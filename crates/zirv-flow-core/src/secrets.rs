//! The operator master key and the per-purpose HMAC keys derived from it.
//!
//! One 32-byte key is held by the operator. Each use derives its own key with HKDF-SHA256,
//! so a key made for one purpose is useless for another. Nothing here prints or returns the
//! master key except [`MasterKey::to_base64`], which exists to write a new key out.

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

const KEY_LEN: usize = 32;

/// Why a master key could not be read. Never contains the rejected input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MasterKeyError;

impl fmt::Display for MasterKeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("not a base64 32-byte key")
    }
}

impl std::error::Error for MasterKeyError {}

/// The operator's 32-byte master key.
pub struct MasterKey([u8; KEY_LEN]);

impl MasterKey {
    /// Reads a key from standard base64, ignoring surrounding whitespace and newlines.
    pub fn parse(text: &str) -> Result<Self, MasterKeyError> {
        let bytes = STANDARD.decode(text.trim()).map_err(|_| MasterKeyError)?;
        let key: [u8; KEY_LEN] = bytes.try_into().map_err(|_| MasterKeyError)?;
        Ok(Self(key))
    }

    /// A new random key.
    ///
    /// # Panics
    ///
    /// If the operating system has no randomness source.
    pub fn generate() -> Self {
        let mut key = [0_u8; KEY_LEN];
        getrandom::fill(&mut key).expect("the operating system provides randomness");
        Self(key)
    }

    /// The key as standard base64, for writing a new key file.
    pub fn to_base64(&self) -> String {
        STANDARD.encode(self.0)
    }

    /// A short public name for the key: the first 8 bytes of its SHA-256, as lowercase hex.
    pub fn key_id(&self) -> String {
        Sha256::digest(self.0)[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// The HMAC key for one purpose: HKDF-SHA256 with an empty salt.
    pub fn hmac_key(&self, purpose: HmacPurpose) -> HmacKey {
        let mut key = [0_u8; KEY_LEN];
        Hkdf::<Sha256>::new(None, &self.0)
            .expand(purpose.info().as_bytes(), &mut key)
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        HmacKey(key)
    }
}

impl fmt::Debug for MasterKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "MasterKey({})", self.key_id())
    }
}

/// What a derived HMAC key is for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HmacPurpose {
    /// Keyed hashes of identifiers, such as e-mail addresses.
    Identifier,
    /// Keyed hashes that stand in for redacted values.
    Redaction,
}

impl HmacPurpose {
    fn info(self) -> &'static str {
        match self {
            Self::Identifier => "zirv-flow/hmac/identifier/v1",
            Self::Redaction => "zirv-flow/hmac/redaction/v1",
        }
    }
}

/// A key derived from the master key for one [`HmacPurpose`].
pub struct HmacKey([u8; KEY_LEN]);

impl HmacKey {
    /// The HMAC-SHA256 of `message`.
    pub fn digest(&self, message: &[u8]) -> [u8; 32] {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&self.0)
            .expect("HMAC accepts a key of any length");
        mac.update(message);
        mac.finalize().into_bytes().into()
    }
}

impl fmt::Debug for HmacKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HmacKey(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed() -> MasterKey {
        MasterKey([7_u8; KEY_LEN])
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn the_key_round_trips_through_base64_and_tolerates_surrounding_whitespace() {
        let key = MasterKey::generate();

        let text = format!("  {}\r\n", key.to_base64());
        let parsed = MasterKey::parse(&text).unwrap();

        assert_eq!(parsed.key_id(), key.key_id());
        assert_eq!(parsed.0, key.0);
    }

    #[test]
    fn generated_keys_differ() {
        assert_ne!(MasterKey::generate().0, MasterKey::generate().0);
    }

    #[test]
    fn a_wrong_length_or_bad_base64_is_rejected_without_echoing_the_input() {
        for input in [
            "",
            "c2hvcnQ=",
            "not base64 !!! hunter2-hunter2-hunter2-hunter2",
            &STANDARD.encode([1_u8; 33]),
        ] {
            let error = MasterKey::parse(input).unwrap_err();

            assert_eq!(error.to_string(), "not a base64 32-byte key");
            assert!(!format!("{error:?}").contains("hunter2"));
        }
    }

    #[test]
    fn the_key_id_is_sixteen_hex_characters_of_the_sha256() {
        let expected = hex(&Sha256::digest([7_u8; KEY_LEN])[..8]);

        assert_eq!(fixed().key_id(), expected);
        assert_eq!(expected.len(), 16);
    }

    #[test]
    fn debug_shows_only_the_key_id() {
        let key = fixed();

        assert_eq!(format!("{key:?}"), format!("MasterKey({})", key.key_id()));
        assert_eq!(
            format!("{:?}", key.hmac_key(HmacPurpose::Identifier)),
            "HmacKey(<redacted>)"
        );
    }

    #[test]
    fn different_purposes_derive_different_keys() {
        let key = fixed();

        let identifier = key.hmac_key(HmacPurpose::Identifier);
        let redaction = key.hmac_key(HmacPurpose::Redaction);

        assert_ne!(identifier.0, redaction.0);
        assert_ne!(identifier.digest(b"a"), redaction.digest(b"a"));
        assert_eq!(identifier.0, key.hmac_key(HmacPurpose::Identifier).0);
    }

    #[test]
    fn a_different_master_key_derives_a_different_key() {
        let other = MasterKey([8_u8; KEY_LEN]);

        assert_ne!(
            fixed().hmac_key(HmacPurpose::Identifier).0,
            other.hmac_key(HmacPurpose::Identifier).0
        );
    }

    #[test]
    fn the_derived_key_and_digest_match_the_known_answers() {
        let key = fixed().hmac_key(HmacPurpose::Identifier);

        assert_eq!(
            hex(&key.0),
            "4ece13a94079c12c7830568ad372048cd67bde363b756e8a9e1f90637f6a0701"
        );
        assert_eq!(
            hex(&key.digest(b"person@example.com")),
            "ab049ff71bfc0bf229a7db6382c360fcff22ae0a54d5bb4127495c0008a76363"
        );
    }

    #[test]
    fn the_redaction_key_and_digest_match_the_known_answers() {
        let key = fixed().hmac_key(HmacPurpose::Redaction);

        assert_eq!(hex(&key.0), "74aaeb1f803248b5f753665c4ef4eecc917622ba5c8a4fe3427673b86f72b7cf");
        assert_eq!(hex(&key.digest(b"person@example.com")), "b635a803eeae338f38ccf4bcadadbac624740b469fe9113628e36f153134bf71");
    }
}
