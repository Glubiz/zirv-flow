//! DKIM signing of an outgoing message (RFC 6376, architecture 10.2): `rsa-sha256` with relaxed
//! header and relaxed body canonicalization.
//!
//! **The signature library is `ring`**, not `rsa`. Both can sign, but `ring`'s RSA private-key
//! operations are constant-time and blinded, which matters for a key that signs on a worker that
//! also serves other work; the `rsa` crate stays for key generation (`zirv_flow_core::sending`),
//! where its timing does not touch an existing secret. `ring` loads the PKCS#8 DER that module
//! stores (RSA-2048), which a test here proves with a generated key.
//!
//! The signer is built by the send pipeline from the sender domain's sealed `dkim_key` secret and
//! handed to the destination through [`Outbound::dkim`](super::Outbound). It holds the parsed key,
//! wiped by `ring` on drop, and has no `Debug` output of the key.

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use sha2::{Digest, Sha256};

/// The headers a signature covers when the message has them, in the order they are hashed. The
/// list is the issue's (From, To, Subject, Date, Message-ID and the `List-Unsubscribe` pair) with
/// the MIME headers, so the part structure cannot be rewritten in transit.
pub(crate) const SIGNED_HEADERS: [&str; 9] = [
    "From",
    "To",
    "Subject",
    "Date",
    "Message-ID",
    "MIME-Version",
    "Content-Type",
    "List-Unsubscribe",
    "List-Unsubscribe-Post",
];

/// The bytes of a base64 signature on one folded line.
const FOLD_AT: usize = 64;

/// A key that cannot sign: not an RSA key `ring` accepts, or a domain or selector that cannot
/// stand in a `d=` or `s=` tag.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DkimSignerError;

impl fmt::Display for DkimSignerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("not a usable DKIM signing key")
    }
}

impl std::error::Error for DkimSignerError {}

/// A domain's DKIM key, parsed and ready to sign.
pub struct DkimSigner {
    domain: String,
    selector: String,
    key: RsaKeyPair,
}

impl DkimSigner {
    /// Parses the PKCS#8 DER private key of `domain`, published under `selector`.
    pub fn from_pkcs8(
        domain: &str,
        selector: &str,
        private_pkcs8: &[u8],
    ) -> Result<Self, DkimSignerError> {
        if !tag_safe(domain) || !tag_safe(selector) {
            return Err(DkimSignerError);
        }
        let key = RsaKeyPair::from_pkcs8(private_pkcs8).map_err(|_| DkimSignerError)?;
        Ok(Self {
            domain: domain.to_ascii_lowercase(),
            selector: selector.to_owned(),
            key,
        })
    }

    /// The domain the signature is made for (`d=`).
    pub fn domain(&self) -> &str {
        &self.domain
    }

    /// The selector the public key is published under (`s=`).
    pub fn selector(&self) -> &str {
        &self.selector
    }

    /// The complete `DKIM-Signature` header, without its trailing line break, for a message with
    /// these `headers` and this `body` (the bytes after the blank line), signed at `timestamp`
    /// (Unix seconds). Only the headers of [`SIGNED_HEADERS`] the message has are signed.
    pub(crate) fn sign(
        &self,
        headers: &[(String, String)],
        body: &[u8],
        timestamp: i64,
    ) -> Result<String, DkimSignerError> {
        let body_hash = STANDARD.encode(Sha256::digest(canonical_body(body)));
        let mut hashed = Vec::new();
        let mut names = Vec::new();
        for wanted in SIGNED_HEADERS {
            if let Some((name, value)) = headers
                .iter()
                .rev()
                .find(|(name, _)| name.eq_ignore_ascii_case(wanted))
            {
                hashed.extend_from_slice(canonical_header(name, value).as_bytes());
                hashed.extend_from_slice(b"\r\n");
                names.push(wanted.to_ascii_lowercase());
            }
        }
        let prefix = format!(
            "v=1; a=rsa-sha256; c=relaxed/relaxed; d={}; s={}; t={timestamp};\r\n\th={};\r\n\tbh={body_hash};\r\n\tb=",
            self.domain,
            self.selector,
            names.join(":"),
        );
        hashed.extend_from_slice(canonical_header("DKIM-Signature", &prefix).as_bytes());

        let mut signature = vec![0; self.key.public().modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                &hashed,
                &mut signature,
            )
            .map_err(|_| DkimSignerError)?;
        let encoded = STANDARD.encode(signature);
        let folded: Vec<&str> = encoded
            .as_bytes()
            .chunks(FOLD_AT)
            .map(|chunk| std::str::from_utf8(chunk).unwrap_or_default())
            .collect();
        Ok(format!("DKIM-Signature: {prefix}{}", folded.join("\r\n\t")))
    }
}

impl fmt::Debug for DkimSigner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "DkimSigner({}, selector {}, [key redacted])",
            self.domain, self.selector
        )
    }
}

/// Whether `text` can sit in a DKIM tag value: a non-empty name of letters, digits, dots and
/// hyphens.
fn tag_safe(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 253
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

/// The relaxed form of a header (RFC 6376 3.4.2), without its line break: the name in lower case,
/// the value unfolded with each run of white space as one space and none at either end.
pub(crate) fn canonical_header(name: &str, value: &str) -> String {
    let mut out = name.to_ascii_lowercase();
    out.push(':');
    let mut gap = false;
    let mut started = false;
    for ch in value.chars() {
        if matches!(ch, ' ' | '\t' | '\r' | '\n') {
            gap = true;
        } else {
            if gap && started {
                out.push(' ');
            }
            gap = false;
            started = true;
            out.push(ch);
        }
    }
    out
}

/// The relaxed form of a body (RFC 6376 3.4.4): trailing white space off each line, each run of
/// white space inside a line one space, and no empty lines at the end. A body that has any text
/// ends in one line break; an empty one stays empty.
pub(crate) fn canonical_body(body: &[u8]) -> Vec<u8> {
    let mut lines: Vec<Vec<u8>> = body
        .split(|byte| *byte == b'\n')
        .map(|line| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let mut out = Vec::with_capacity(line.len());
            let mut gap = false;
            for byte in line {
                if matches!(byte, b' ' | b'\t') {
                    gap = true;
                } else {
                    if gap {
                        out.push(b' ');
                    }
                    gap = false;
                    out.push(*byte);
                }
            }
            out
        })
        .collect();
    while lines.last().is_some_and(Vec::is_empty) {
        lines.pop();
    }
    let mut out = Vec::new();
    for line in lines {
        out.extend_from_slice(&line);
        out.extend_from_slice(b"\r\n");
    }
    out
}

#[cfg(any(test, feature = "test-support"))]
mod verification {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use rsa::pkcs8::DecodePublicKey;
    use rsa::{Pkcs1v15Sign, RsaPublicKey};
    use sha2::{Digest, Sha256};
    use zirv_flow_core::sending::dkim_record_public_key;

    use super::{canonical_body, canonical_header};

    /// The DER prefix of a SHA-256 `DigestInfo`.
    const SHA256_PREFIX: [u8; 19] = [
        0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01,
        0x05, 0x00, 0x04, 0x20,
    ];

    /// What a verifier finds wrong with a message's signature; none when it verifies against the
    /// public key in `record_text`, the text a domain publishes. An independent reading of
    /// RFC 6376: it parses the tags, recomputes the body hash and the header hash and checks the
    /// RSA signature with the `rsa` crate.
    pub fn verify_signature(message: &[u8], record_text: &str) -> Result<(), String> {
        let split = message
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .ok_or("no blank line")?;
        let head = String::from_utf8_lossy(&message[..split]).into_owned();
        let body = &message[split + 4..];

        // Unfold the header block into logical headers.
        let mut headers: Vec<(String, String)> = Vec::new();
        for line in head.split("\r\n") {
            if line.starts_with([' ', '\t']) {
                let last = headers.last_mut().ok_or("fold before any header")?;
                last.1.push_str("\r\n");
                last.1.push_str(line);
            } else {
                let (name, value) = line.split_once(':').ok_or("header without a colon")?;
                headers.push((name.to_owned(), value.to_owned()));
            }
        }
        let (_, raw) = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("DKIM-Signature"))
            .ok_or("no DKIM-Signature")?;
        let tags: Vec<(String, String)> = raw
            .split(';')
            .filter_map(|tag| tag.split_once('='))
            .map(|(name, value)| {
                (
                    name.trim().to_owned(),
                    value.split_whitespace().collect::<String>(),
                )
            })
            .collect();
        let tag = |name: &str| {
            tags.iter()
                .find(|(tag, _)| tag == name)
                .map(|(_, value)| value.clone())
                .ok_or(format!("no {name}= tag"))
        };
        if tag("v")? != "1" || tag("a")? != "rsa-sha256" || tag("c")? != "relaxed/relaxed" {
            return Err("unexpected v, a or c".into());
        }
        let body_hash = STANDARD.encode(Sha256::digest(canonical_body(body)));
        if body_hash != tag("bh")? {
            return Err("the body hash does not match".into());
        }

        let mut hashed = Vec::new();
        for name in tag("h")?.split(':') {
            let (found, value) = headers
                .iter()
                .rev()
                .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
                .ok_or(format!("signed header {name} is absent"))?;
            hashed.extend_from_slice(canonical_header(found, value).as_bytes());
            hashed.extend_from_slice(b"\r\n");
        }
        // The signature header with the value of b= removed.
        let value_of_b = raw
            .rfind("b=")
            .filter(|at| raw[..*at].ends_with(|c: char| c.is_whitespace() || c == ';'))
            .ok_or("no b= tag")?;
        let without = format!("{}b=", &raw[..value_of_b]);
        hashed.extend_from_slice(canonical_header("DKIM-Signature", &without).as_bytes());

        let signature = STANDARD.decode(tag("b")?).map_err(|_| "b= is not base64")?;
        let public = dkim_record_public_key(record_text).ok_or("the record has no key")?;
        let der = STANDARD.decode(public).map_err(|_| "p= is not base64")?;
        let key = RsaPublicKey::from_public_key_der(&der).map_err(|_| "p= is not a public key")?;
        let scheme = Pkcs1v15Sign {
            hash_len: Some(32),
            prefix: SHA256_PREFIX.to_vec().into_boxed_slice(),
        };
        key.verify(scheme, &Sha256::digest(&hashed), &signature)
            .map_err(|_| "the RSA signature does not verify".to_owned())
    }
}

#[cfg(any(test, feature = "test-support"))]
pub use verification::verify_signature;

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::OnceLock;
    use zirv_flow_core::sending::{DkimKey, dkim_record_text, generate_dkim_key};

    use super::*;

    /// One generated key for the tests of this crate: generation takes a while unoptimised.
    pub(crate) fn key() -> &'static DkimKey {
        static KEY: OnceLock<DkimKey> = OnceLock::new();
        KEY.get_or_init(|| generate_dkim_key().unwrap())
    }

    pub(crate) fn signer(domain: &str) -> DkimSigner {
        DkimSigner::from_pkcs8(domain, "zirv", key().private_pkcs8.expose()).unwrap()
    }

    pub(crate) use super::verify_signature as verify;

    fn headers() -> Vec<(String, String)> {
        vec![
            ("From".into(), "News <news@example.com>".into()),
            ("To".into(), "<ada@example.net>".into()),
            ("Subject".into(), "Hello".into()),
            ("Date".into(), "Thu, 01 Jan 2032 00:00:00 +0000".into()),
            ("X-Other".into(), "not signed".into()),
        ]
    }

    #[test]
    fn the_stored_key_loads_into_ring_and_a_signature_verifies_against_the_published_record() {
        let signer = signer("example.com");
        let record = dkim_record_text(&key().public_key);
        let body = b"Hi there  \r\n\r\n";
        let header = signer.sign(&headers(), body, 1_956_528_000).unwrap();

        let mut message = Vec::new();
        for (name, value) in headers() {
            message.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        message.extend_from_slice(format!("{header}\r\n\r\n").as_bytes());
        message.extend_from_slice(body);

        assert_eq!(verify(&message, &record), Ok(()));
        assert!(header.starts_with("DKIM-Signature: v=1; a=rsa-sha256; c=relaxed/relaxed;"));
        assert!(header.contains("h=from:to:subject:date;"));
    }

    #[test]
    fn a_changed_header_or_body_no_longer_verifies() {
        let signer = signer("example.com");
        let record = dkim_record_text(&key().public_key);
        let header = signer.sign(&headers(), b"Hi\r\n", 1).unwrap();
        let build = |subject: &str, body: &str| {
            let mut message = format!(
                "From: News <news@example.com>\r\nTo: <ada@example.net>\r\nSubject: {subject}\r\nDate: Thu, 01 Jan 2032 00:00:00 +0000\r\n{header}\r\n\r\n{body}"
            );
            message.truncate(message.len());
            message.into_bytes()
        };

        assert_eq!(verify(&build("Hello", "Hi\r\n"), &record), Ok(()));
        assert!(verify(&build("Hullo", "Hi\r\n"), &record).is_err());
        assert!(verify(&build("Hello", "Hi!\r\n"), &record).is_err());
    }

    #[test]
    fn relaxed_canonicalization_follows_the_rfc_examples() {
        assert_eq!(
            canonical_header("SubJect", " \tMy  \r\n  test\t "),
            "subject:My test"
        );
        assert_eq!(
            canonical_body(b" C \r\nD \t E\r\n\r\n\r\n"),
            b" C\r\nD E\r\n".to_vec()
        );
        assert!(canonical_body(b"").is_empty());
        assert!(canonical_body(b"\r\n\r\n").is_empty());
    }

    #[test]
    fn a_key_that_ring_cannot_load_or_an_unsafe_name_is_refused_and_debug_hides_the_key() {
        assert!(DkimSigner::from_pkcs8("example.com", "zirv", b"nope").is_err());
        assert!(
            DkimSigner::from_pkcs8("exa mple.com", "zirv", key().private_pkcs8.expose()).is_err()
        );
        assert!(
            DkimSigner::from_pkcs8("example.com", "a;b", key().private_pkcs8.expose()).is_err()
        );

        let signer = signer("Example.COM");

        assert_eq!(signer.domain(), "example.com");
        assert_eq!(signer.selector(), "zirv");
        assert!(format!("{signer:?}").contains("[key redacted]"));
    }
}
