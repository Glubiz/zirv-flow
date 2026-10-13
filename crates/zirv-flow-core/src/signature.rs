//! The HMAC-SHA256 signature of an inbound webhook (architecture sections 7.1 and 16.1).
//!
//! Two schemes share one shape. The timestamped scheme signs `"{timestamp}.{body}"`, where the
//! timestamp is the decimal unix seconds the sender put in a header, and rejects a timestamp
//! outside a tolerance in either direction. The body-only scheme signs the raw body and has no
//! replay defense of its own. The signature header holds a fixed prefix followed by 64 hex
//! digits (any case); the key is the secret's UTF-8 bytes. Pure: the header lookup is a closure
//! so this crate names no HTTP type, and no error holds an input.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use crate::SecretString;

type HmacSha256 = Hmac<Sha256>;

/// The longest header name a scheme accepts.
pub const MAX_HEADER_NAME_CHARS: usize = 64;
/// The length of a SHA-256 digest in hex digits.
const DIGEST_HEX_LEN: usize = 64;

/// How a hook's requests are signed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Scheme<'a> {
    /// The header holding the signature.
    pub signature_header: &'a str,
    /// The text before the hex digits in the signature header; may be empty.
    pub signature_prefix: &'a str,
    /// The header holding the unix seconds the sender signed; `None` selects the body-only
    /// scheme.
    pub timestamp_header: Option<&'a str>,
    /// How far the timestamp may be from now, in seconds, in either direction.
    pub tolerance_seconds: i64,
}

/// A request that passed [`verify`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Verified {
    /// The signed timestamp, when the scheme has one.
    pub signed_at: Option<i64>,
}

/// Why a request did not verify. Holds no input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignatureError {
    MissingSignature,
    MalformedSignature,
    MissingTimestamp,
    MalformedTimestamp,
    OutsideTolerance,
    Mismatch,
}

impl SignatureError {
    /// A static code, safe for a metric label.
    pub const fn code(self) -> &'static str {
        match self {
            Self::MissingSignature => "signature_missing",
            Self::MalformedSignature => "signature_malformed",
            Self::MissingTimestamp => "timestamp_missing",
            Self::MalformedTimestamp => "timestamp_malformed",
            Self::OutsideTolerance => "timestamp_outside_tolerance",
            Self::Mismatch => "signature_mismatch",
        }
    }
}

impl std::fmt::Display for SignatureError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for SignatureError {}

/// The MAC over `"{timestamp}.{body}"`; the timestamp is the header text as the sender signed it.
fn mac(secret: &[u8], timestamp: Option<&str>, body: &[u8]) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts a key of any length");
    if let Some(timestamp) = timestamp {
        mac.update(timestamp.as_bytes());
        mac.update(b".");
    }
    mac.update(body);
    mac
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_digest(hex: &str) -> Option<Vec<u8>> {
    let bytes = hex.as_bytes();
    if bytes.len() != DIGEST_HEX_LEN {
        return None;
    }
    bytes
        .chunks(2)
        .map(|pair| Some((hex_value(pair[0])? << 4) | hex_value(pair[1])?))
        .collect()
}

/// Checks one request. The cheap checks run first (header present, prefix and hex shape,
/// timestamp present and numeric, tolerance); the MAC is computed only after they pass and is
/// compared in constant time.
pub fn verify<'h>(
    scheme: &Scheme<'_>,
    secret: &[u8],
    header: &dyn Fn(&str) -> Option<&'h str>,
    body: &[u8],
    now_unix: i64,
) -> Result<Verified, SignatureError> {
    let value = header(scheme.signature_header).ok_or(SignatureError::MissingSignature)?;
    let digest = value
        .strip_prefix(scheme.signature_prefix)
        .and_then(decode_digest)
        .ok_or(SignatureError::MalformedSignature)?;
    let (signed_at, signed_text) = match scheme.timestamp_header {
        None => (None, None),
        Some(name) => {
            let text = header(name).ok_or(SignatureError::MissingTimestamp)?;
            let digits = !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
            let timestamp = digits
                .then(|| text.parse::<i64>().ok())
                .flatten()
                .ok_or(SignatureError::MalformedTimestamp)?;
            if now_unix.abs_diff(timestamp) > scheme.tolerance_seconds.max(0).unsigned_abs() {
                return Err(SignatureError::OutsideTolerance);
            }
            (Some(timestamp), Some(text))
        }
    };
    mac(secret, signed_text, body)
        .verify_slice(&digest)
        .map_err(|_| SignatureError::Mismatch)?;
    Ok(Verified { signed_at })
}

/// The signature digest for `body`: 64 lowercase hex digits, without the prefix. Pass the
/// timestamp for the timestamped scheme. For tests, docs and the UI's curl example.
pub fn sign(secret: &[u8], timestamp: Option<i64>, body: &[u8]) -> String {
    mac(
        secret,
        timestamp.map(|seconds| seconds.to_string()).as_deref(),
        body,
    )
    .finalize()
    .into_bytes()
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect()
}

/// A new shared secret: `zfwh_` and 32 random bytes as 64 hex digits.
///
/// # Panics
///
/// If the operating system has no randomness source.
pub fn generate_secret() -> SecretString {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).expect("the operating system provides randomness");
    let mut text = String::from("zfwh_");
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    SecretString::new(text)
}

/// Whether `name` is an HTTP token of at most [`MAX_HEADER_NAME_CHARS`] characters.
pub fn check_header_name(name: &str) -> bool {
    const SYMBOLS: &[u8] = b"!#$%&'*+-.^_`|~";
    !name.is_empty()
        && name.len() <= MAX_HEADER_NAME_CHARS
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || SYMBOLS.contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"zfwh_secret";
    const BODY: &[u8] = br#"{"email":"a@example.test"}"#;
    const NOW: i64 = 1_700_000_000;

    fn timestamped() -> Scheme<'static> {
        Scheme {
            signature_header: "X-Sig",
            signature_prefix: "sha256=",
            timestamp_header: Some("X-Ts"),
            tolerance_seconds: 300,
        }
    }

    fn body_only() -> Scheme<'static> {
        Scheme {
            timestamp_header: None,
            ..timestamped()
        }
    }

    fn run(
        scheme: &Scheme<'_>,
        secret: &[u8],
        headers: &[(&str, String)],
        body: &[u8],
    ) -> Result<Verified, SignatureError> {
        let lookup = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        };
        verify(scheme, secret, &lookup, body, NOW)
    }

    fn signed(timestamp: i64) -> Vec<(&'static str, String)> {
        vec![
            (
                "x-sig",
                format!("sha256={}", sign(SECRET, Some(timestamp), BODY)),
            ),
            ("x-ts", timestamp.to_string()),
        ]
    }

    #[test]
    fn a_zero_padded_timestamp_header_signed_verbatim_verifies() {
        let padded = format!("0{NOW}");
        let mut mac = HmacSha256::new_from_slice(SECRET).unwrap();
        mac.update(format!("{padded}.").as_bytes());
        mac.update(BODY);
        let digest: String = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let headers = [("x-sig", format!("sha256={digest}")), ("x-ts", padded)];

        let verified = run(&timestamped(), SECRET, &headers, BODY).unwrap();
        assert_eq!(verified.signed_at, Some(NOW));
    }

    #[test]
    fn a_valid_timestamped_request_verifies() {
        let verified = run(&timestamped(), SECRET, &signed(NOW), BODY).unwrap();
        assert_eq!(verified.signed_at, Some(NOW));
    }

    #[test]
    fn a_valid_body_only_request_verifies() {
        let headers = vec![("X-Sig", format!("sha256={}", sign(SECRET, None, BODY)))];
        let verified = run(&body_only(), SECRET, &headers, BODY).unwrap();
        assert_eq!(verified.signed_at, None);
    }

    #[test]
    fn a_wrong_secret_or_altered_body_is_a_mismatch() {
        assert_eq!(
            run(&timestamped(), b"other", &signed(NOW), BODY),
            Err(SignatureError::Mismatch)
        );
        assert_eq!(
            run(&timestamped(), SECRET, &signed(NOW), b"{}"),
            Err(SignatureError::Mismatch)
        );
    }

    #[test]
    fn an_altered_timestamp_is_a_mismatch() {
        let mut headers = signed(NOW);
        headers[1].1 = (NOW + 1).to_string();
        assert_eq!(
            run(&timestamped(), SECRET, &headers, BODY),
            Err(SignatureError::Mismatch)
        );
    }

    #[test]
    fn the_timestamp_is_part_of_the_signed_message() {
        let headers = vec![
            ("x-sig", format!("sha256={}", sign(SECRET, None, BODY))),
            ("x-ts", NOW.to_string()),
        ];
        assert_eq!(
            run(&timestamped(), SECRET, &headers, BODY),
            Err(SignatureError::Mismatch)
        );
    }

    #[test]
    fn a_missing_signature_header_is_reported() {
        assert_eq!(
            run(&timestamped(), SECRET, &[("x-ts", NOW.to_string())], BODY),
            Err(SignatureError::MissingSignature)
        );
    }

    #[test]
    fn the_prefix_must_match() {
        let digest = sign(SECRET, Some(NOW), BODY);
        for value in [
            digest.clone(),
            format!("sha1={digest}"),
            format!("SHA256={digest}"),
        ] {
            let headers = vec![("x-sig", value), ("x-ts", NOW.to_string())];
            assert_eq!(
                run(&timestamped(), SECRET, &headers, BODY),
                Err(SignatureError::MalformedSignature)
            );
        }
    }

    #[test]
    fn an_empty_prefix_takes_a_bare_digest() {
        let scheme = Scheme {
            signature_prefix: "",
            ..body_only()
        };
        let headers = vec![("x-sig", sign(SECRET, None, BODY))];
        assert!(run(&scheme, SECRET, &headers, BODY).is_ok());
        let headers = vec![("x-sig", format!("sha256={}", sign(SECRET, None, BODY)))];
        assert_eq!(
            run(&scheme, SECRET, &headers, BODY),
            Err(SignatureError::MalformedSignature)
        );
    }

    #[test]
    fn a_digest_that_is_not_64_hex_digits_is_malformed() {
        let digest = sign(SECRET, Some(NOW), BODY);
        let non_hex = format!("{}zz", &digest[..62]);
        for value in [
            String::new(),
            digest[..63].to_owned(),
            format!("{digest}0"),
            non_hex,
        ] {
            let headers = vec![
                ("x-sig", format!("sha256={value}")),
                ("x-ts", NOW.to_string()),
            ];
            assert_eq!(
                run(&timestamped(), SECRET, &headers, BODY),
                Err(SignatureError::MalformedSignature),
                "{value}"
            );
        }
    }

    #[test]
    fn uppercase_hex_is_accepted() {
        let headers = vec![
            (
                "x-sig",
                format!("sha256={}", sign(SECRET, Some(NOW), BODY).to_uppercase()),
            ),
            ("x-ts", NOW.to_string()),
        ];
        assert!(run(&timestamped(), SECRET, &headers, BODY).is_ok());
    }

    #[test]
    fn a_missing_or_non_numeric_timestamp_is_reported() {
        let signature = ("x-sig", format!("sha256={}", sign(SECRET, Some(NOW), BODY)));
        assert_eq!(
            run(
                &timestamped(),
                SECRET,
                std::slice::from_ref(&signature),
                BODY
            ),
            Err(SignatureError::MissingTimestamp)
        );
        for text in [
            "",
            "abc",
            "-5",
            "+5",
            "1.5",
            " 5",
            "99999999999999999999999",
        ] {
            let headers = vec![signature.clone(), ("x-ts", text.to_owned())];
            assert_eq!(
                run(&timestamped(), SECRET, &headers, BODY),
                Err(SignatureError::MalformedTimestamp),
                "{text}"
            );
        }
    }

    #[test]
    fn the_tolerance_edge_is_accepted_and_one_second_past_is_not() {
        for offset in [-300, 300, 0] {
            assert!(run(&timestamped(), SECRET, &signed(NOW + offset), BODY).is_ok());
        }
        for offset in [-301, 301] {
            assert_eq!(
                run(&timestamped(), SECRET, &signed(NOW + offset), BODY),
                Err(SignatureError::OutsideTolerance)
            );
        }
    }

    #[test]
    fn the_tolerance_is_checked_before_the_mac() {
        let headers = vec![
            ("x-sig", format!("sha256={}", "0".repeat(64))),
            ("x-ts", (NOW - 9999).to_string()),
        ];
        assert_eq!(
            run(&timestamped(), SECRET, &headers, BODY),
            Err(SignatureError::OutsideTolerance)
        );
    }

    #[test]
    fn the_mac_alone_decides_between_a_right_and_a_wrong_well_formed_signature() {
        let right = signed(NOW);
        let mut wrong = signed(NOW);
        let flipped = if wrong[0].1.ends_with('0') { '1' } else { '0' };
        wrong[0].1.pop();
        wrong[0].1.push(flipped);
        assert!(run(&timestamped(), SECRET, &right, BODY).is_ok());
        assert_eq!(
            run(&timestamped(), SECRET, &wrong, BODY),
            Err(SignatureError::Mismatch)
        );
    }

    #[test]
    fn errors_hold_no_input() {
        for error in [
            SignatureError::MissingSignature,
            SignatureError::MalformedSignature,
            SignatureError::MissingTimestamp,
            SignatureError::MalformedTimestamp,
            SignatureError::OutsideTolerance,
            SignatureError::Mismatch,
        ] {
            // A unit variant: its Debug is its name and its code is static text.
            assert!(!format!("{error:?}").contains(char::is_numeric));
            assert!(error.code().is_ascii());
        }
    }

    #[test]
    fn generated_secrets_are_prefixed_long_and_unique() {
        let one = generate_secret();
        let two = generate_secret();
        assert!(one.expose().starts_with("zfwh_"));
        assert_eq!(one.expose().len(), 5 + 64);
        assert!(
            one.expose()[5..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        );
        assert_ne!(one.expose(), two.expose());
    }

    #[test]
    fn header_names_must_be_http_tokens() {
        assert!(check_header_name("X-Zirv-Signature"));
        assert!(check_header_name("x_sig.1"));
        assert!(!check_header_name(""));
        assert!(!check_header_name("has space"));
        assert!(!check_header_name("colon:"));
        assert!(!check_header_name("new\nline"));
        assert!(!check_header_name("\u{e9}"));
        assert!(check_header_name(&"a".repeat(64)));
        assert!(!check_header_name(&"a".repeat(65)));
    }

    /// The README's example: `crates/.../README.md` documents this secret, timestamp, body and
    /// digest, so the example works as written.
    #[test]
    fn the_documented_example_has_the_documented_digest() {
        let body = br#"{"id":"order-1","email":"ada@example.com"}"#;
        let digest = sign(b"zfwh_example", Some(1_700_000_000), body);
        assert_eq!(digest, README_EXAMPLE_DIGEST);
    }

    const README_EXAMPLE_DIGEST: &str =
        "c8447361d430f2281aef6579aec897c4930f3d15af1920d3da1f83ff8e4f4d56";
}
