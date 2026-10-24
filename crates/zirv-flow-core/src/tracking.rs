//! The tokens in a sent message's open pixel, tracked links and unsubscribe links (architecture
//! 10.2).
//!
//! A token is base64url, without padding, of 35 bytes: a kind byte, the 16-byte message id, a
//! big-endian `u16` link index (0 for an open and for an unsubscribe) and the first 16 bytes of the
//! HMAC-SHA256 of those 19 bytes under the tracking key. It holds no personal data: the message id
//! is a random identifier, and what it leads to is looked up by the server.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use uuid::Uuid;

use crate::HmacKey;
use crate::token::constant_time_eq;

/// What a token may be used for. A token only works on the route of its own kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TokenKind {
    /// The open pixel.
    Open,
    /// A tracked link.
    Click,
    /// The unsubscribe and preference pages.
    Unsubscribe,
}

impl TokenKind {
    fn byte(self) -> u8 {
        match self {
            Self::Open => b'o',
            Self::Click => b'c',
            Self::Unsubscribe => b'u',
        }
    }
}

/// The bytes the MAC covers: kind, message id, link index.
const BODY_LEN: usize = 1 + 16 + 2;
/// How much of the HMAC a token carries.
const MAC_LEN: usize = 16;

/// A verified token's contents.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Claims {
    pub message: Uuid,
    pub link: u16,
}

fn mac(key: &HmacKey, body: &[u8]) -> [u8; MAC_LEN] {
    let digest = key.digest(body);
    let mut out = [0_u8; MAC_LEN];
    out.copy_from_slice(&digest[..MAC_LEN]);
    out
}

/// The token of `kind` for `message` and link `link`.
pub fn sign(key: &HmacKey, kind: TokenKind, message: Uuid, link: u16) -> String {
    let mut bytes = Vec::with_capacity(BODY_LEN + MAC_LEN);
    bytes.push(kind.byte());
    bytes.extend_from_slice(message.as_bytes());
    bytes.extend_from_slice(&link.to_be_bytes());
    let tag = mac(key, &bytes);
    bytes.extend_from_slice(&tag);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The contents of `token` when it is of `kind` and its MAC holds; otherwise none. The MAC is
/// compared in constant time.
pub fn verify(key: &HmacKey, kind: TokenKind, token: &str) -> Option<Claims> {
    let bytes = URL_SAFE_NO_PAD.decode(token).ok()?;
    if bytes.len() != BODY_LEN + MAC_LEN {
        return None;
    }
    let (body, tag) = bytes.split_at(BODY_LEN);
    let valid = constant_time_eq(&mac(key, body), tag);
    if !valid || body[0] != kind.byte() {
        return None;
    }
    let message = Uuid::from_slice(&body[1..17]).ok()?;
    let link = u16::from_be_bytes([body[17], body[18]]);
    Some(Claims { message, link })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HmacPurpose, HmacRoot, MasterKey};

    fn key(seed: &str) -> HmacKey {
        let master = MasterKey::parse(&{
            let mut raw = [0_u8; 32];
            raw[0] = seed.as_bytes()[0];
            base64::engine::general_purpose::STANDARD.encode(raw)
        })
        .unwrap();
        HmacRoot::bootstrap_from(&master).hmac_key(HmacPurpose::Tracking)
    }

    fn message() -> Uuid {
        Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef)
    }

    #[test]
    fn a_token_round_trips_its_message_and_link() {
        let key = key("a");
        for (kind, link) in [
            (TokenKind::Open, 0),
            (TokenKind::Click, 7),
            (TokenKind::Click, u16::MAX),
            (TokenKind::Unsubscribe, 0),
        ] {
            let token = sign(&key, kind, message(), link);
            assert_eq!(
                verify(&key, kind, &token),
                Some(Claims {
                    message: message(),
                    link
                })
            );
        }
    }

    #[test]
    fn a_tampered_byte_is_refused_wherever_it_is() {
        let key = key("a");
        let token = sign(&key, TokenKind::Click, message(), 3);
        let bytes = URL_SAFE_NO_PAD.decode(&token).unwrap();
        for at in 0..bytes.len() {
            let mut changed = bytes.clone();
            changed[at] ^= 1;
            assert_eq!(
                verify(&key, TokenKind::Click, &URL_SAFE_NO_PAD.encode(changed)),
                None,
                "byte {at}"
            );
        }
    }

    #[test]
    fn the_wrong_kind_key_or_length_is_refused() {
        let key = key("a");
        let token = sign(&key, TokenKind::Open, message(), 0);

        assert_eq!(verify(&key, TokenKind::Click, &token), None);
        assert_eq!(verify(&key, TokenKind::Unsubscribe, &token), None);
        assert_eq!(verify(&self::key("b"), TokenKind::Open, &token), None);
        let bytes = URL_SAFE_NO_PAD.decode(&token).unwrap();
        for cut in [0, 1, bytes.len() - 1] {
            assert_eq!(
                verify(
                    &key,
                    TokenKind::Open,
                    &URL_SAFE_NO_PAD.encode(&bytes[..cut])
                ),
                None
            );
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert_eq!(
            verify(&key, TokenKind::Open, &URL_SAFE_NO_PAD.encode(longer)),
            None
        );
        for junk in ["", "x", "not base64!", "====", &"A".repeat(200)] {
            assert_eq!(verify(&key, TokenKind::Open, junk), None, "{junk}");
        }
    }

    #[test]
    fn the_decoded_bytes_hold_only_the_kind_the_message_and_the_index() {
        let key = key("a");
        let token = sign(&key, TokenKind::Click, message(), 258);
        let bytes = URL_SAFE_NO_PAD.decode(&token).unwrap();

        assert_eq!(bytes.len(), 35);
        assert_eq!(bytes[0], b'c');
        assert_eq!(&bytes[1..17], message().as_bytes());
        assert_eq!(&bytes[17..19], &[1, 2]);
        assert!(!token.contains('='));
    }
}
