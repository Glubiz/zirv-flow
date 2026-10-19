//! The operator master key, envelope encryption of stored secrets, and the HMAC keys.
//!
//! One 32-byte key is held by the operator. It never encrypts data directly: a key encryption
//! key is derived from it with HKDF-SHA256, and that wraps a fresh random data key per secret
//! (XChaCha20-Poly1305). Rotating the master key re-wraps data keys and leaves ciphertexts alone.
//! The HMAC keys derive from a stable [`HmacRoot`] held in the database, not from the master
//! key, so a rotation cannot change an identifier, suppression or redaction hash. Nothing here
//! prints or returns the master key except [`MasterKey::to_base64`], which exists to write a
//! new key out.

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Deserializer};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

use crate::{SecretId, WorkspaceId};

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;
/// The associated data of every wrapped data key.
const WRAP_AAD: &[u8] = b"zirv-flow/dk/v1";
const KEK_INFO: &[u8] = b"zirv-flow/kek/v1";

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

    /// The key encryption key: HKDF-SHA256 over the key bytes. Never stored.
    fn kek(&self) -> Zeroizing<[u8; KEY_LEN]> {
        let mut kek = Zeroizing::new([0_u8; KEY_LEN]);
        Hkdf::<Sha256>::new(None, &self.0)
            .expand(KEK_INFO, kek.as_mut())
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        kek
    }
}

impl Drop for MasterKey {
    fn drop(&mut self) {
        self.0.zeroize();
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

/// The stable root every [`HmacKey`] derives from.
///
/// It is created once per deployment from the master key that produced the existing hashes,
/// then stored sealed, so a master key rotation re-wraps it and never changes it.
pub struct HmacRoot([u8; KEY_LEN]);

impl HmacRoot {
    /// The root for a deployment that has none yet: a copy of the master key bytes, so hashes
    /// made before the root existed stay valid.
    pub fn bootstrap_from(key: &MasterKey) -> Self {
        Self(key.0)
    }

    /// A root from its stored bytes.
    pub fn from_bytes(bytes: &SecretBytes) -> Result<Self, SecretError> {
        let root: [u8; KEY_LEN] = bytes
            .expose()
            .try_into()
            .map_err(|_| SecretError::Corrupt)?;
        Ok(Self(root))
    }

    /// The root bytes, to be sealed for storage.
    pub fn to_secret_bytes(&self) -> SecretBytes {
        SecretBytes::new(self.0.to_vec())
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

impl Drop for HmacRoot {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for HmacRoot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HmacRoot(<redacted>)")
    }
}

/// A key derived from the [`HmacRoot`] for one [`HmacPurpose`].
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

impl Drop for HmacKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for HmacKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HmacKey(<redacted>)")
    }
}

/// Why a keyring could not be built.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyringError;

impl fmt::Display for KeyringError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the previous master key is the same as the current one")
    }
}

impl std::error::Error for KeyringError {}

/// The master keys this process can use: the current one, and during a rotation the one it
/// replaces.
#[derive(Debug)]
pub struct Keyring {
    current: MasterKey,
    previous: Option<MasterKey>,
}

impl Keyring {
    /// A previous key equal to the current one is rejected.
    pub fn new(current: MasterKey, previous: Option<MasterKey>) -> Result<Self, KeyringError> {
        if previous.as_ref().is_some_and(|old| old.0 == current.0) {
            return Err(KeyringError);
        }
        Ok(Self { current, previous })
    }

    /// The key that seals and wraps.
    pub fn current(&self) -> &MasterKey {
        &self.current
    }

    /// Whether a previous key is configured.
    pub fn has_previous(&self) -> bool {
        self.previous.is_some()
    }

    /// The ids of the configured keys: the current one, then the previous one if any.
    pub fn key_ids(&self) -> Vec<String> {
        [Some(&self.current), self.previous.as_ref()]
            .into_iter()
            .flatten()
            .map(MasterKey::key_id)
            .collect()
    }

    /// The configured key with this id.
    pub fn key(&self, key_id: &str) -> Option<&MasterKey> {
        [Some(&self.current), self.previous.as_ref()]
            .into_iter()
            .flatten()
            .find(|key| key.key_id() == key_id)
    }
}

/// Why a secret could not be sealed or opened. Never contains key or plaintext bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SecretError {
    /// The data key is wrapped by a master key this process does not hold.
    UnknownKey { key_id: String },
    /// The stored value is malformed, was tampered with, or belongs to another row.
    Corrupt,
}

impl fmt::Display for SecretError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownKey { key_id } => write!(
                formatter,
                "the secret is wrapped by master key {key_id}, which is not configured; \
                 set secrets.previous_master_key_file to the key with that id"
            ),
            Self::Corrupt => formatter.write_str("the secret could not be decrypted"),
        }
    }
}

impl std::error::Error for SecretError {}

/// What a stored secret is for. Stored as text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecretPurpose {
    /// A password or token a connector uses to reach a source.
    ConnectorCredential,
    /// An OAuth access or refresh token.
    OAuthToken,
    /// The API key of an AI provider.
    AiProviderKey,
    /// The shared secret that signs an inbound webhook.
    WebhookSecret,
    /// The per-workspace secret an `action.webhook` call is signed with.
    WebhookSigning,
}

impl SecretPurpose {
    /// The stored text.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConnectorCredential => "connector_credential",
            Self::OAuthToken => "oauth_token",
            Self::AiProviderKey => "ai_provider_key",
            Self::WebhookSecret => "webhook_secret",
            Self::WebhookSigning => "webhook_signing",
        }
    }

    /// The purpose with this stored text.
    pub fn parse(text: &str) -> Option<Self> {
        [
            Self::ConnectorCredential,
            Self::OAuthToken,
            Self::AiProviderKey,
            Self::WebhookSecret,
            Self::WebhookSigning,
        ]
        .into_iter()
        .find(|purpose| purpose.as_str() == text)
    }
}

/// Plaintext bytes that are wiped on drop. No `Display`, `Serialize` or `Clone`; read them
/// with [`SecretBytes::expose`].
pub struct SecretBytes(Zeroizing<Vec<u8>>);

impl SecretBytes {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(Zeroizing::new(bytes))
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SecretBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted]")
    }
}

/// Plaintext text that is wiped on drop. No `Display`, `Serialize` or `Clone`; it can be
/// deserialized, so a request body lands in it directly. Read it with
/// [`SecretString::expose`].
pub struct SecretString(Zeroizing<String>);

impl SecretString {
    pub fn new(text: String) -> Self {
        Self(Zeroizing::new(text))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::new)
    }
}

impl AsRef<str> for SecretString {
    fn as_ref(&self) -> &str {
        self.expose()
    }
}

impl TryFrom<SecretBytes> for SecretString {
    type Error = SecretError;

    fn try_from(bytes: SecretBytes) -> Result<Self, SecretError> {
        let text = std::str::from_utf8(bytes.expose()).map_err(|_| SecretError::Corrupt)?;
        Ok(Self::new(text.to_owned()))
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted]")
    }
}

/// A sealed secret as stored: everything needed to open it except the master key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Sealed {
    /// The id of the master key that wrapped the data key.
    pub key_id: String,
    /// The wrapped data key: a 24-byte nonce followed by the ciphertext.
    pub wrapped_key: Vec<u8>,
    /// The nonce of the payload encryption.
    pub nonce: [u8; NONCE_LEN],
    pub ciphertext: Vec<u8>,
}

/// The associated data of a workspace secret, which ties the ciphertext to its row.
pub fn secret_aad(workspace: WorkspaceId, secret: SecretId, purpose: SecretPurpose) -> Vec<u8> {
    format!(
        "zirv-flow/secret/v1|{workspace}|{secret}|{}",
        purpose.as_str()
    )
    .into_bytes()
}

/// The associated data of a deployment key.
pub fn deployment_key_aad(name: &str) -> Vec<u8> {
    format!("zirv-flow/deployment-key/v1|{name}").into_bytes()
}

fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0_u8; N];
    getrandom::fill(&mut bytes).expect("the operating system provides randomness");
    bytes
}

fn cipher(key: &[u8; KEY_LEN]) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new(&Key::from(*key))
}

fn nonce(bytes: &[u8; NONCE_LEN]) -> XNonce {
    XNonce::from(*bytes)
}

fn wrap(kek: &[u8; KEY_LEN], data_key: &[u8; KEY_LEN]) -> Vec<u8> {
    let nonce_bytes = random::<NONCE_LEN>();
    let wrapped = cipher(kek)
        .encrypt(
            &nonce(&nonce_bytes),
            Payload {
                msg: data_key,
                aad: WRAP_AAD,
            },
        )
        .expect("encrypting 32 bytes cannot fail");
    [nonce_bytes.as_slice(), &wrapped].concat()
}

fn unwrap_key(keyring: &Keyring, sealed: &Sealed) -> Result<Zeroizing<[u8; KEY_LEN]>, SecretError> {
    let master = keyring
        .key(&sealed.key_id)
        .ok_or_else(|| SecretError::UnknownKey {
            key_id: sealed.key_id.clone(),
        })?;
    if sealed.wrapped_key.len() != NONCE_LEN + KEY_LEN + TAG_LEN {
        return Err(SecretError::Corrupt);
    }
    let (wrap_nonce, wrapped) = sealed.wrapped_key.split_at(NONCE_LEN);
    let wrap_nonce: [u8; NONCE_LEN] = wrap_nonce.try_into().map_err(|_| SecretError::Corrupt)?;
    let kek = master.kek();
    let bytes = Zeroizing::new(
        cipher(&kek)
            .decrypt(
                &nonce(&wrap_nonce),
                Payload {
                    msg: wrapped,
                    aad: WRAP_AAD,
                },
            )
            .map_err(|_| SecretError::Corrupt)?,
    );
    let mut data_key = Zeroizing::new([0_u8; KEY_LEN]);
    data_key.copy_from_slice(&bytes);
    Ok(data_key)
}

/// Encrypts `plaintext` under a fresh data key and wraps that key with the current master key.
/// `aad` is authenticated, not stored: opening needs the same value.
pub fn seal(keyring: &Keyring, plaintext: &[u8], aad: &[u8]) -> Sealed {
    let data_key = Zeroizing::new(random::<KEY_LEN>());
    let payload_nonce = random::<NONCE_LEN>();
    let ciphertext = cipher(&data_key)
        .encrypt(
            &nonce(&payload_nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .expect("encryption cannot fail");
    let kek = keyring.current().kek();
    Sealed {
        key_id: keyring.current().key_id(),
        wrapped_key: wrap(&kek, &data_key),
        nonce: payload_nonce,
        ciphertext,
    }
}

/// Decrypts a sealed secret with the key named by its `key_id`.
pub fn open(keyring: &Keyring, sealed: &Sealed, aad: &[u8]) -> Result<SecretBytes, SecretError> {
    let data_key = unwrap_key(keyring, sealed)?;
    cipher(&data_key)
        .decrypt(
            &nonce(&sealed.nonce),
            Payload {
                msg: &sealed.ciphertext,
                aad,
            },
        )
        .map(SecretBytes::new)
        .map_err(|_| SecretError::Corrupt)
}

/// Wraps the data key with the current master key. The nonce and ciphertext are unchanged.
pub fn rewrap(keyring: &Keyring, sealed: &Sealed) -> Result<Sealed, SecretError> {
    let data_key = unwrap_key(keyring, sealed)?;
    let kek = keyring.current().kek();
    Ok(Sealed {
        key_id: keyring.current().key_id(),
        wrapped_key: wrap(&kek, &data_key),
        nonce: sealed.nonce,
        ciphertext: sealed.ciphertext.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed() -> MasterKey {
        MasterKey([7_u8; KEY_LEN])
    }

    fn root() -> HmacRoot {
        HmacRoot::bootstrap_from(&fixed())
    }

    fn keyring(current: u8, previous: Option<u8>) -> Keyring {
        Keyring::new(
            MasterKey([current; KEY_LEN]),
            previous.map(|byte| MasterKey([byte; KEY_LEN])),
        )
        .unwrap()
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
            format!("{:?}", root().hmac_key(HmacPurpose::Identifier)),
            "HmacKey(<redacted>)"
        );
    }

    #[test]
    fn different_purposes_derive_different_keys() {
        let identifier = root().hmac_key(HmacPurpose::Identifier);
        let redaction = root().hmac_key(HmacPurpose::Redaction);

        assert_ne!(identifier.0, redaction.0);
        assert_ne!(identifier.digest(b"a"), redaction.digest(b"a"));
        assert_eq!(identifier.0, root().hmac_key(HmacPurpose::Identifier).0);
    }

    #[test]
    fn a_different_master_key_derives_a_different_key() {
        let other = MasterKey([8_u8; KEY_LEN]);

        assert_ne!(
            root().hmac_key(HmacPurpose::Identifier).0,
            HmacRoot::bootstrap_from(&other)
                .hmac_key(HmacPurpose::Identifier)
                .0
        );
    }

    #[test]
    fn the_derived_key_and_digest_match_the_known_answers() {
        let key = root().hmac_key(HmacPurpose::Identifier);

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
        let key = root().hmac_key(HmacPurpose::Redaction);

        assert_eq!(
            hex(&key.0),
            "74aaeb1f803248b5f753665c4ef4eecc917622ba5c8a4fe3427673b86f72b7cf"
        );
        assert_eq!(
            hex(&key.digest(b"person@example.com")),
            "b635a803eeae338f38ccf4bcadadbac624740b469fe9113628e36f153134bf71"
        );
    }

    fn aad() -> Vec<u8> {
        secret_aad(
            WorkspaceId::new(),
            SecretId::new(),
            SecretPurpose::ConnectorCredential,
        )
    }

    #[test]
    fn a_sealed_secret_opens_to_the_plaintext() {
        let keys = keyring(1, None);
        let aad = aad();

        let sealed = seal(&keys, b"hunter2-hunter2", &aad);

        assert_eq!(sealed.key_id, keys.current().key_id());
        assert_eq!(sealed.wrapped_key.len(), NONCE_LEN + KEY_LEN + TAG_LEN);
        assert!(!sealed.ciphertext.windows(7).any(|w| w == b"hunter2"));
        assert_eq!(
            open(&keys, &sealed, &aad).unwrap().expose(),
            b"hunter2-hunter2"
        );
    }

    #[test]
    fn two_seals_of_one_plaintext_share_nothing() {
        let keys = keyring(1, None);

        let first = seal(&keys, b"same", b"aad");
        let second = seal(&keys, b"same", b"aad");

        assert_ne!(first.nonce, second.nonce);
        assert_ne!(first.wrapped_key, second.wrapped_key);
        assert_ne!(first.ciphertext, second.ciphertext);
    }

    #[test]
    fn a_wrong_aad_a_flipped_ciphertext_bit_and_a_flipped_wrapped_key_bit_each_fail() {
        let keys = keyring(1, None);
        let sealed = seal(&keys, b"secret", b"row-a");

        assert_eq!(
            open(&keys, &sealed, b"row-b").unwrap_err(),
            SecretError::Corrupt
        );

        let mut flipped = sealed.clone();
        flipped.ciphertext[0] ^= 1;
        assert_eq!(
            open(&keys, &flipped, b"row-a").unwrap_err(),
            SecretError::Corrupt
        );

        let mut flipped = sealed.clone();
        flipped.wrapped_key[NONCE_LEN + 3] ^= 1;
        assert_eq!(
            open(&keys, &flipped, b"row-a").unwrap_err(),
            SecretError::Corrupt
        );

        let mut flipped = sealed.clone();
        flipped.nonce[0] ^= 1;
        assert_eq!(
            open(&keys, &flipped, b"row-a").unwrap_err(),
            SecretError::Corrupt
        );

        let mut short = sealed;
        short.wrapped_key.truncate(10);
        assert_eq!(
            open(&keys, &short, b"row-a").unwrap_err(),
            SecretError::Corrupt
        );
    }

    #[test]
    fn the_workspace_secret_aad_binds_the_workspace_row_and_purpose() {
        let workspace = WorkspaceId::new();
        let secret = SecretId::new();
        let base = secret_aad(workspace, secret, SecretPurpose::OAuthToken);

        assert_eq!(
            String::from_utf8(base.clone()).unwrap(),
            format!("zirv-flow/secret/v1|{workspace}|{secret}|oauth_token")
        );
        assert_ne!(
            base,
            secret_aad(WorkspaceId::new(), secret, SecretPurpose::OAuthToken)
        );
        assert_ne!(
            base,
            secret_aad(workspace, SecretId::new(), SecretPurpose::OAuthToken)
        );
        assert_ne!(
            base,
            secret_aad(workspace, secret, SecretPurpose::AiProviderKey)
        );
        assert_eq!(
            deployment_key_aad("hmac_root"),
            b"zirv-flow/deployment-key/v1|hmac_root"
        );
    }

    #[test]
    fn an_unknown_key_id_is_named_with_the_config_key_to_set() {
        let sealed = seal(&keyring(1, None), b"secret", b"aad");
        let other = keyring(2, None);

        let error = open(&other, &sealed, b"aad").unwrap_err();

        let message = error.to_string();
        assert!(message.contains(&sealed.key_id), "{message}");
        assert!(
            message.contains("secrets.previous_master_key_file"),
            "{message}"
        );
        assert_eq!(rewrap(&other, &sealed).unwrap_err(), error);
    }

    #[test]
    fn a_corrupt_error_reveals_nothing() {
        let keys = keyring(1, None);
        let sealed = seal(&keys, b"hunter2", b"a");

        let error = open(&keys, &sealed, b"b").unwrap_err();

        assert_eq!(error.to_string(), "the secret could not be decrypted");
    }

    #[test]
    fn rewrap_keeps_the_ciphertext_and_nonce_and_opens_with_the_new_key_alone() {
        let old = keyring(1, None);
        let sealed = seal(&old, b"secret", b"aad");
        let both = keyring(2, Some(1));

        let rewrapped = rewrap(&both, &sealed).unwrap();

        assert_eq!(rewrapped.key_id, both.current().key_id());
        assert_ne!(rewrapped.wrapped_key, sealed.wrapped_key);
        assert_eq!(rewrapped.nonce, sealed.nonce);
        assert_eq!(rewrapped.ciphertext, sealed.ciphertext);
        let new_only = keyring(2, None);
        assert_eq!(
            open(&new_only, &rewrapped, b"aad").unwrap().expose(),
            b"secret"
        );
        assert!(open(&old, &rewrapped, b"aad").is_err());
    }

    #[test]
    fn a_secret_wrapped_by_the_previous_key_opens_while_it_is_configured() {
        let sealed = seal(&keyring(1, None), b"secret", b"aad");

        let both = keyring(2, Some(1));

        assert_eq!(open(&both, &sealed, b"aad").unwrap().expose(), b"secret");
        assert!(both.has_previous());
    }

    #[test]
    fn a_previous_key_equal_to_the_current_one_is_rejected() {
        let error =
            Keyring::new(MasterKey([1; KEY_LEN]), Some(MasterKey([1; KEY_LEN]))).unwrap_err();

        assert_eq!(
            error.to_string(),
            "the previous master key is the same as the current one"
        );
    }

    #[test]
    fn the_keyring_finds_keys_by_id() {
        let keys = keyring(2, Some(1));

        assert_eq!(
            keys.key(&MasterKey([1; KEY_LEN]).key_id()).unwrap().0,
            [1; KEY_LEN]
        );
        assert_eq!(keys.key(&keys.current().key_id()).unwrap().0, [2; KEY_LEN]);
        assert!(keys.key("0000000000000000").is_none());
    }

    #[test]
    fn the_kek_is_not_the_master_key_and_differs_per_key() {
        let key = fixed();

        assert_ne!(*key.kek(), key.0);
        assert_ne!(*key.kek(), *MasterKey([8; KEY_LEN]).kek());
    }

    #[test]
    fn the_bootstrapped_root_derives_exactly_the_keys_the_master_key_used_to() {
        let root = HmacRoot::bootstrap_from(&fixed());

        // Both vectors were captured from `MasterKey::hmac_key` before the root existed.
        let identifier = root.hmac_key(HmacPurpose::Identifier);
        let redaction = root.hmac_key(HmacPurpose::Redaction);

        assert_eq!(
            hex(&identifier.0),
            "4ece13a94079c12c7830568ad372048cd67bde363b756e8a9e1f90637f6a0701"
        );
        assert_eq!(
            hex(&redaction.0),
            "74aaeb1f803248b5f753665c4ef4eecc917622ba5c8a4fe3427673b86f72b7cf"
        );
    }

    #[test]
    fn the_root_survives_sealing_and_a_rotation() {
        let old = keyring(7, None);
        let root = HmacRoot::bootstrap_from(old.current());
        let name = deployment_key_aad("hmac_root");
        let sealed = seal(&old, root.to_secret_bytes().expose(), &name);

        let rewrapped = rewrap(&keyring(9, Some(7)), &sealed).unwrap();
        let opened = open(&keyring(9, None), &rewrapped, &name).unwrap();
        let reloaded = HmacRoot::from_bytes(&opened).unwrap();

        assert_eq!(
            reloaded.hmac_key(HmacPurpose::Identifier).digest(b"a"),
            root.hmac_key(HmacPurpose::Identifier).digest(b"a")
        );
        assert!(HmacRoot::from_bytes(&SecretBytes::new(vec![1, 2, 3])).is_err());
        assert_eq!(format!("{root:?}"), "HmacRoot(<redacted>)");
    }

    #[test]
    fn the_secret_types_print_as_redacted() {
        let bytes = SecretBytes::new(b"hunter2".to_vec());
        let text = SecretString::new("hunter2".to_owned());

        assert_eq!(format!("{bytes:?}"), "[redacted]");
        assert_eq!(format!("{text:?}"), "[redacted]");
        assert_eq!(text.expose(), "hunter2");
        assert_eq!(bytes.expose(), b"hunter2");
    }

    #[test]
    fn secret_bytes_become_a_string_only_when_they_are_utf8() {
        let ok = SecretString::try_from(SecretBytes::new(b"abc".to_vec())).unwrap();

        assert_eq!(ok.expose(), "abc");
        assert_eq!(
            SecretString::try_from(SecretBytes::new(vec![0xff, 0xfe])).unwrap_err(),
            SecretError::Corrupt
        );
    }

    // The probe is ambiguous, so it fails to compile, for any type that is `Serialize` or `Clone`.
    #[test]
    fn the_secret_types_can_be_neither_serialized_nor_cloned() {
        trait NotSerialize<A> {
            fn check() {}
        }
        impl<T: ?Sized> NotSerialize<()> for T {}
        impl<T: ?Sized + serde::Serialize> NotSerialize<u8> for T {}

        trait NotClone<A> {
            fn check() {}
        }
        impl<T: ?Sized> NotClone<()> for T {}
        impl<T: Clone> NotClone<u8> for T {}

        <SecretString as NotSerialize<_>>::check();
        <SecretBytes as NotSerialize<_>>::check();
        <SecretString as NotClone<_>>::check();
        <SecretBytes as NotClone<_>>::check();
    }

    #[test]
    fn purposes_round_trip_through_their_stored_text() {
        for purpose in [
            SecretPurpose::ConnectorCredential,
            SecretPurpose::OAuthToken,
            SecretPurpose::AiProviderKey,
            SecretPurpose::WebhookSecret,
            SecretPurpose::WebhookSigning,
        ] {
            assert_eq!(SecretPurpose::parse(purpose.as_str()), Some(purpose));
        }
        assert_eq!(SecretPurpose::parse("other"), None);
    }
}
