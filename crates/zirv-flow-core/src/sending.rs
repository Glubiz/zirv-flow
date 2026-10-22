//! Sending domains (architecture 10.2 and 16.2): the names, the DNS record texts a domain
//! publishes, the rules that decide whether what DNS returned is what was asked for, and DKIM key
//! generation.
//!
//! Pure: nothing here resolves a name. A resolver hands [`check_record`] the TXT strings it
//! found. The RSA key is generated here and never used to sign or decrypt (signing is done with
//! `ring` in the SMTP destination, which loads the PKCS#8 key this module produces).

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rsa::RsaPrivateKey;
use rsa::pkcs8::{DecodePrivateKey, DecodePublicKey, EncodePrivateKey, EncodePublicKey};
use rsa::traits::PublicKeyParts;

use crate::{SecretBytes, ValidationError};

/// The modulus size of a generated DKIM key. 2048 bits is the size mail receivers expect.
pub const DKIM_KEY_BITS: usize = 2048;

/// The selector a domain gets when the request names none.
pub const DEFAULT_SELECTOR: &str = "zirv";

const MAX_DOMAIN: usize = 253;
const MAX_LABEL: usize = 63;
const MAX_MECHANISM: usize = 255;

/// Which of a domain's three DNS records.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RecordKind {
    Dkim,
    Spf,
    Dmarc,
}

impl RecordKind {
    /// Every kind, in the order a domain's records are listed.
    pub const ALL: [Self; 3] = [Self::Dkim, Self::Spf, Self::Dmarc];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dkim => "dkim",
            Self::Spf => "spf",
            Self::Dmarc => "dmarc",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == text)
    }
}

/// What the last check of one record found.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RecordStatus {
    /// Not checked yet.
    Pending,
    /// DNS holds a record that satisfies the rule.
    Ok,
    /// DNS holds a record of the right kind that does not satisfy the rule.
    Mismatch,
    /// DNS holds no record of the right kind.
    Missing,
}

impl RecordStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ok => "ok",
            Self::Mismatch => "mismatch",
            Self::Missing => "missing",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [Self::Pending, Self::Ok, Self::Mismatch, Self::Missing]
            .into_iter()
            .find(|status| status.as_str() == text)
    }
}

fn is_label_char(character: char) -> bool {
    character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
}

fn is_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= MAX_LABEL
        && label.chars().all(is_label_char)
        && !label.starts_with('-')
        && !label.ends_with('-')
}

/// A domain name to send from: trimmed, lower-cased, one trailing dot removed, at least two
/// labels of letters, digits and hyphens. Internationalized names are given in their punycode
/// form.
pub fn parse_domain(value: &str) -> Result<String, ValidationError> {
    let invalid = |message| ValidationError::new("domain", message);
    let lowered = value.trim().to_lowercase();
    let value = lowered.strip_suffix('.').unwrap_or(&lowered);
    if !value.is_ascii() {
        return Err(invalid(
            "must be ASCII; give an internationalized name in punycode",
        ));
    }
    if value.len() > MAX_DOMAIN {
        return Err(invalid("is too long"));
    }
    let mut labels = 0_usize;
    let mut last = "";
    for label in value.split('.') {
        if !is_label(label) {
            return Err(invalid("is not a valid domain name"));
        }
        labels += 1;
        last = label;
    }
    if labels < 2 || last.chars().all(|character| character.is_ascii_digit()) {
        return Err(invalid(
            "must have at least two labels and a non-numeric top label",
        ));
    }
    Ok(value.to_owned())
}

/// A DKIM selector: 1 to 63 lower-case letters, digits and hyphens.
pub fn parse_selector(value: &str) -> Result<String, ValidationError> {
    let value = value.trim().to_ascii_lowercase();
    if is_label(&value) {
        Ok(value)
    } else {
        Err(ValidationError::new(
            "selector",
            "must be 1 to 63 letters, digits or hyphens, not starting or ending with a hyphen",
        ))
    }
}

/// The SPF mechanism the domain's record must contain, such as `include:spf.relay.example`,
/// `ip4:203.0.113.7`, `a` or `mx`. Lower-cased.
pub fn parse_spf_mechanism(value: &str) -> Result<String, ValidationError> {
    let invalid = || {
        ValidationError::new(
            "spf_mechanism",
            "must be one SPF mechanism: include:, ip4:, ip6:, a, mx, a:<name> or mx:<name>",
        )
    };
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty() || value.len() > MAX_MECHANISM {
        return Err(invalid());
    }
    let allowed = |character: char| {
        character.is_ascii_alphanumeric() || matches!(character, '.' | ':' | '/' | '_' | '-')
    };
    if !value.chars().all(allowed) {
        return Err(invalid());
    }
    let known = match value.split_once(':') {
        None => matches!(value.as_str(), "a" | "mx"),
        Some((name, rest)) => {
            !rest.is_empty() && matches!(name, "include" | "ip4" | "ip6" | "a" | "mx")
        }
    };
    if known { Ok(value) } else { Err(invalid()) }
}

/// The name a record is published at.
pub fn record_host(kind: RecordKind, selector: &str, domain: &str) -> String {
    match kind {
        RecordKind::Dkim => format!("{selector}._domainkey.{domain}"),
        RecordKind::Spf => domain.to_owned(),
        RecordKind::Dmarc => format!("_dmarc.{domain}"),
    }
}

/// The DKIM TXT value for a base64 SubjectPublicKeyInfo public key.
pub fn dkim_record_text(public_key: &str) -> String {
    format!("v=DKIM1; k=rsa; p={public_key}")
}

/// The recommended SPF TXT value. It must be merged with the domain's existing SPF record: a
/// name holds one `v=spf1` record, and two of them make SPF fail.
pub fn spf_record_text(mechanism: Option<&str>) -> String {
    match mechanism {
        Some(mechanism) => format!("v=spf1 {mechanism} ~all"),
        None => "v=spf1 ~all".to_owned(),
    }
}

/// The recommended DMARC TXT value: monitor only, until the operator tightens the policy.
pub fn dmarc_record_text() -> String {
    "v=DMARC1; p=none".to_owned()
}

/// The `tag=value` pairs of a DKIM or DMARC record, tags lower-cased and everything trimmed.
fn tags(text: &str) -> Vec<(String, String)> {
    text.split(';')
        .filter_map(|part| part.split_once('='))
        .map(|(tag, value)| (tag.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect()
}

fn tag<'a>(tags: &'a [(String, String)], name: &str) -> Option<&'a str> {
    tags.iter()
        .find(|(tag, _)| tag == name)
        .map(|(_, value)| value.as_str())
}

/// The `p=` value of a DKIM record with its whitespace removed (a long key is often published as
/// several strings, and a host may fold it).
fn dkim_public_key(text: &str) -> Option<String> {
    let tags = tags(text);
    if tag(&tags, "v").is_some_and(|version| !version.eq_ignore_ascii_case("DKIM1")) {
        return None;
    }
    if tag(&tags, "k").is_some_and(|kind| !kind.eq_ignore_ascii_case("rsa")) {
        return None;
    }
    tag(&tags, "p").map(|key| key.chars().filter(|c| !c.is_whitespace()).collect())
}

fn is_spf(text: &str) -> bool {
    let text = text.trim();
    text.get(..6)
        .is_some_and(|head| head.eq_ignore_ascii_case("v=spf1"))
        && text[6..].chars().next().is_none_or(char::is_whitespace)
}

fn is_all_term(term: &str) -> bool {
    term.trim_start_matches(['+', '-', '~', '?'])
        .eq_ignore_ascii_case("all")
}

fn is_dmarc_prefix(text: &str) -> bool {
    let first = text.split(';').next().unwrap_or("");
    first.split_once('=').is_some_and(|(tag, value)| {
        tag.trim().eq_ignore_ascii_case("v") && value.trim() == "DMARC1"
    })
}

fn is_valid_dmarc(text: &str) -> bool {
    let tags = tags(text);
    is_dmarc_prefix(text)
        && tag(&tags, "p").is_some_and(|policy| {
            matches!(
                policy.to_ascii_lowercase().as_str(),
                "none" | "quarantine" | "reject"
            )
        })
}

/// Decides one record. `expected` is the text the domain was told to publish; `observed` are the
/// TXT values found at the record's name, one string per record (a record's character strings
/// already joined).
///
/// - **DKIM:** a record counts when its `v` (if any) is `DKIM1`, its `k` (if any) is `rsa`, and
///   its `p=` equals the expected `p=` once whitespace is removed. No TXT at the name is
///   `Missing`; TXT without a matching key is `Mismatch`.
/// - **SPF:** exactly one `v=spf1` record must exist (none is `Missing`; two are `Mismatch`, as
///   SPF treats them as an error), and it must contain every mechanism of the expected text
///   other than the closing `all` term, compared case-insensitively. An expected text with no
///   mechanism accepts any `v=spf1` record.
/// - **DMARC:** any record whose first tag is `v=DMARC1` and whose `p=` is `none`,
///   `quarantine` or `reject`. No `v=DMARC1` record is `Missing`; one that is not valid is
///   `Mismatch`.
pub fn check_record(kind: RecordKind, expected: &str, observed: &[String]) -> RecordStatus {
    match kind {
        RecordKind::Dkim => {
            if observed.is_empty() {
                return RecordStatus::Missing;
            }
            let Some(wanted) = dkim_public_key(expected) else {
                return RecordStatus::Mismatch;
            };
            let found = observed.iter().any(|text| {
                dkim_public_key(text).is_some_and(|key| !key.is_empty() && key == wanted)
            });
            if found {
                RecordStatus::Ok
            } else {
                RecordStatus::Mismatch
            }
        }
        RecordKind::Spf => {
            let found: Vec<&String> = observed.iter().filter(|text| is_spf(text)).collect();
            match found.as_slice() {
                [] => RecordStatus::Missing,
                [only] => {
                    let have: Vec<&str> = only.split_whitespace().collect();
                    let satisfied = expected
                        .split_whitespace()
                        .skip(1)
                        .filter(|term| !is_all_term(term))
                        .all(|term| have.iter().any(|h| h.eq_ignore_ascii_case(term)));
                    if satisfied {
                        RecordStatus::Ok
                    } else {
                        RecordStatus::Mismatch
                    }
                }
                _ => RecordStatus::Mismatch,
            }
        }
        RecordKind::Dmarc => {
            let found: Vec<&String> = observed
                .iter()
                .filter(|text| is_dmarc_prefix(text))
                .collect();
            if found.is_empty() {
                RecordStatus::Missing
            } else if found.iter().any(|text| is_valid_dmarc(text)) {
                RecordStatus::Ok
            } else {
                RecordStatus::Mismatch
            }
        }
    }
}

/// Why a DKIM key could not be made or read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DkimKeyError;

impl std::fmt::Display for DkimKeyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("not a usable RSA key")
    }
}

impl std::error::Error for DkimKeyError {}

/// A new DKIM key pair.
pub struct DkimKey {
    /// The private key as PKCS#8 DER, wiped on drop. Sealed into `encrypted_secrets` by the
    /// caller; never returned by any API.
    pub private_pkcs8: SecretBytes,
    /// The public key as base64 SubjectPublicKeyInfo DER: the `p=` value.
    pub public_key: String,
}

impl std::fmt::Debug for DkimKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DkimKey(..)")
    }
}

fn public_of(key: &RsaPrivateKey) -> Result<String, DkimKeyError> {
    let der = key
        .to_public_key()
        .to_public_key_der()
        .map_err(|_| DkimKeyError)?;
    Ok(STANDARD.encode(der.as_bytes()))
}

/// Generates an RSA-2048 key. CPU-bound (hundreds of milliseconds, more unoptimised), so async
/// callers run it on a blocking thread.
pub fn generate_dkim_key() -> Result<DkimKey, DkimKeyError> {
    let key =
        RsaPrivateKey::new(&mut rand::rngs::OsRng, DKIM_KEY_BITS).map_err(|_| DkimKeyError)?;
    let der = key.to_pkcs8_der().map_err(|_| DkimKeyError)?;
    Ok(DkimKey {
        private_pkcs8: SecretBytes::new(der.as_bytes().to_vec()),
        public_key: public_of(&key)?,
    })
}

/// The public key (base64 SubjectPublicKeyInfo) of a PKCS#8 private key.
pub fn dkim_public_key_of(private_pkcs8: &[u8]) -> Result<String, DkimKeyError> {
    let key = RsaPrivateKey::from_pkcs8_der(private_pkcs8).map_err(|_| DkimKeyError)?;
    public_of(&key)
}

/// The modulus size in bits of the public key in a DKIM record's `p=` value.
pub fn dkim_record_key_bits(public_key: &str) -> Result<usize, DkimKeyError> {
    let der = STANDARD.decode(public_key).map_err(|_| DkimKeyError)?;
    let key = rsa::RsaPublicKey::from_public_key_der(&der).map_err(|_| DkimKeyError)?;
    Ok(key.size() * 8)
}

/// The `p=` value of a published DKIM record, if it is a DKIM record with a key.
pub fn dkim_record_public_key(text: &str) -> Option<String> {
    dkim_public_key(text).filter(|key| !key.is_empty())
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use super::*;

    fn key() -> &'static DkimKey {
        static KEY: OnceLock<DkimKey> = OnceLock::new();
        KEY.get_or_init(|| generate_dkim_key().unwrap())
    }

    fn texts(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn a_domain_is_lower_cased_and_must_have_two_valid_labels() {
        assert_eq!(
            parse_domain(" Mail.Example.COM. ").unwrap(),
            "mail.example.com"
        );
        for bad in [
            "",
            "localhost",
            "a..b",
            "-a.com",
            "a-.com",
            "a b.com",
            "exämple.com",
            "1.2",
            "a@b.com",
            "a_b.com",
        ] {
            assert_eq!(parse_domain(bad).unwrap_err().field(), "domain", "{bad:?}");
        }
        assert!(parse_domain(&format!("{}.com", "a".repeat(64))).is_err());
        assert!(parse_domain(&format!("{}.com", "a".repeat(63))).is_ok());
    }

    #[test]
    fn a_selector_is_one_label() {
        assert_eq!(parse_selector(" Zirv-1 ").unwrap(), "zirv-1");
        assert!(parse_selector("a.b").is_err());
        assert!(parse_selector("").is_err());
        assert!(parse_selector("-x").is_err());
    }

    #[test]
    fn an_spf_mechanism_is_one_known_term() {
        for good in [
            "include:spf.relay.example",
            "IP4:203.0.113.7/32",
            "a",
            "mx",
            "mx:mail.example.com",
        ] {
            assert!(parse_spf_mechanism(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "all",
            "include:",
            "include:a b",
            "redirect=x.com",
            "v=spf1",
            "ptr",
        ] {
            assert!(parse_spf_mechanism(bad).is_err(), "{bad}");
        }
        assert_eq!(
            parse_spf_mechanism("INCLUDE:X.Example").unwrap(),
            "include:x.example"
        );
    }

    #[test]
    fn records_are_published_at_their_names() {
        assert_eq!(
            record_host(RecordKind::Dkim, "zirv", "example.com"),
            "zirv._domainkey.example.com"
        );
        assert_eq!(
            record_host(RecordKind::Spf, "zirv", "example.com"),
            "example.com"
        );
        assert_eq!(
            record_host(RecordKind::Dmarc, "zirv", "example.com"),
            "_dmarc.example.com"
        );
    }

    #[test]
    fn the_recommended_texts_pass_their_own_checks() {
        let dkim = dkim_record_text(&key().public_key);
        let spf = spf_record_text(Some("include:spf.relay.example"));
        let dmarc = dmarc_record_text();

        assert_eq!(
            check_record(RecordKind::Dkim, &dkim, std::slice::from_ref(&dkim)),
            RecordStatus::Ok
        );
        assert_eq!(
            check_record(RecordKind::Spf, &spf, std::slice::from_ref(&spf)),
            RecordStatus::Ok
        );
        assert_eq!(
            check_record(RecordKind::Dmarc, &dmarc, std::slice::from_ref(&dmarc)),
            RecordStatus::Ok
        );
        assert_eq!(spf_record_text(None), "v=spf1 ~all");
    }

    #[test]
    fn the_dkim_record_parses_back_to_the_public_key_of_the_private_key() {
        let key = key();
        let text = dkim_record_text(&key.public_key);

        let published = dkim_record_public_key(&text).unwrap();

        assert_eq!(
            published,
            dkim_public_key_of(key.private_pkcs8.expose()).unwrap()
        );
        assert_eq!(dkim_record_key_bits(&published).unwrap(), DKIM_KEY_BITS);
        assert!(!text.contains(&STANDARD.encode(key.private_pkcs8.expose())));
    }

    #[test]
    fn a_dkim_record_is_compared_on_its_key_ignoring_folding_and_tag_case() {
        let expected = dkim_record_text("AAAABBBB");

        assert_eq!(
            check_record(
                RecordKind::Dkim,
                &expected,
                &texts(&["V=DKIM1;  K=rsa; P=AAAA BBBB ;"])
            ),
            RecordStatus::Ok
        );
        assert_eq!(
            check_record(RecordKind::Dkim, &expected, &texts(&["p=AAAABBBB"])),
            RecordStatus::Ok
        );
    }

    #[test]
    fn a_dkim_record_with_another_key_a_revoked_key_or_another_type_is_a_mismatch() {
        let expected = dkim_record_text("AAAA");
        for found in [
            "v=DKIM1; k=rsa; p=BBBB",
            "v=DKIM1; p=",
            "v=DKIM2; p=AAAA",
            "k=ed25519; p=AAAA",
            "hello",
        ] {
            assert_eq!(
                check_record(RecordKind::Dkim, &expected, &texts(&[found])),
                RecordStatus::Mismatch,
                "{found}"
            );
        }
        assert_eq!(
            check_record(RecordKind::Dkim, &expected, &[]),
            RecordStatus::Missing
        );
    }

    #[test]
    fn spf_needs_exactly_one_record_with_the_configured_mechanism() {
        let expected = spf_record_text(Some("include:spf.relay.example"));
        let check = |found: &[&str]| check_record(RecordKind::Spf, &expected, &texts(found));

        assert_eq!(
            check(&["v=spf1 mx INCLUDE:SPF.relay.example -all"]),
            RecordStatus::Ok
        );
        assert_eq!(check(&["v=spf1 mx -all"]), RecordStatus::Mismatch);
        assert_eq!(
            check(&["v=spf1 include:spf.relay.example", "v=spf1 mx"]),
            RecordStatus::Mismatch
        );
        assert_eq!(
            check(&["google-site-verification=x", "v=spf10 a"]),
            RecordStatus::Missing
        );
        assert_eq!(check(&[]), RecordStatus::Missing);
    }

    #[test]
    fn an_spf_text_without_a_mechanism_accepts_any_spf_record() {
        let expected = spf_record_text(None);

        assert_eq!(
            check_record(RecordKind::Spf, &expected, &texts(&["v=spf1 -all"])),
            RecordStatus::Ok
        );
        assert_eq!(
            check_record(RecordKind::Spf, &expected, &texts(&["v=spf1"])),
            RecordStatus::Ok
        );
    }

    #[test]
    fn dmarc_accepts_any_valid_record_and_ignores_other_text() {
        let expected = dmarc_record_text();
        let check = |found: &[&str]| check_record(RecordKind::Dmarc, &expected, &texts(found));

        assert_eq!(
            check(&["other", "v=DMARC1; p=reject; rua=mailto:a@example.com"]),
            RecordStatus::Ok
        );
        assert_eq!(check(&["v=DMARC1;p=Quarantine"]), RecordStatus::Ok);
        assert_eq!(
            check(&["v=DMARC1; rua=mailto:a@example.com"]),
            RecordStatus::Mismatch
        );
        assert_eq!(check(&["v=DMARC1; p=maybe"]), RecordStatus::Mismatch);
        assert_eq!(check(&["p=none; v=DMARC1"]), RecordStatus::Missing);
        assert_eq!(check(&["unrelated"]), RecordStatus::Missing);
        assert_eq!(check(&[]), RecordStatus::Missing);
    }

    #[test]
    fn kinds_and_statuses_round_trip_through_their_text() {
        for kind in RecordKind::ALL {
            assert_eq!(RecordKind::parse(kind.as_str()), Some(kind));
        }
        for status in [
            RecordStatus::Pending,
            RecordStatus::Ok,
            RecordStatus::Mismatch,
            RecordStatus::Missing,
        ] {
            assert_eq!(RecordStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(RecordKind::parse("txt"), None);
    }

    #[test]
    fn a_pkcs8_key_that_is_not_rsa_is_refused() {
        assert_eq!(dkim_public_key_of(b"nonsense"), Err(DkimKeyError));
        assert_eq!(dkim_record_key_bits("%%%"), Err(DkimKeyError));
    }

    #[test]
    fn debug_output_hides_the_key() {
        let text = format!("{:?}", key());

        assert!(!text.contains(&key().public_key));
    }
}
