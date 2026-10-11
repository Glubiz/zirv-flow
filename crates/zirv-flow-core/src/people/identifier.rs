//! Identifier kinds and their normalization (architecture section 7.3).

use std::fmt;

use crate::HmacKey;

const MAX_VALUE_CHARS: usize = 256;
const MAX_NAMESPACE_CHARS: usize = 64;

/// What an identifier identifies a person by.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum IdentifierKind {
    /// An id from a named outside system, written `external:<namespace>`.
    External(String),
    Email,
    Phone,
    AnonymousId,
}

/// Why an identifier was refused. Names the rule, never the value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentifierError {
    UnknownKind,
    InvalidNamespace,
    InvalidEmail,
    InvalidPhone,
    InvalidValue,
}

impl IdentifierError {
    /// A stable, static reason for decision records and API answers.
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::UnknownKind => "unknown_identifier_kind",
            Self::InvalidNamespace => "invalid_identifier_namespace",
            Self::InvalidEmail => "invalid_email",
            Self::InvalidPhone => "invalid_phone",
            Self::InvalidValue => "invalid_identifier_value",
        }
    }
}

impl fmt::Display for IdentifierError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.reason())
    }
}

impl std::error::Error for IdentifierError {}

impl IdentifierKind {
    /// Parses `email`, `phone`, `anonymous_id` or `external:<namespace>`.
    pub fn parse(text: &str) -> Result<Self, IdentifierError> {
        match text {
            "email" => Ok(Self::Email),
            "phone" => Ok(Self::Phone),
            "anonymous_id" => Ok(Self::AnonymousId),
            _ => {
                let namespace = text
                    .strip_prefix("external:")
                    .ok_or(IdentifierError::UnknownKind)?;
                let valid = !namespace.is_empty()
                    && namespace.len() <= MAX_NAMESPACE_CHARS
                    && namespace.bytes().all(|byte| {
                        byte.is_ascii_lowercase()
                            || byte.is_ascii_digit()
                            || matches!(byte, b'_' | b'.' | b'-')
                    });
                if valid {
                    Ok(Self::External(namespace.to_owned()))
                } else {
                    Err(IdentifierError::InvalidNamespace)
                }
            }
        }
    }

    /// The stored text form.
    pub fn as_text(&self) -> String {
        match self {
            Self::External(namespace) => format!("external:{namespace}"),
            Self::Email => "email".to_owned(),
            Self::Phone => "phone".to_owned(),
            Self::AnonymousId => "anonymous_id".to_owned(),
        }
    }

    /// How strongly the kind identifies a person; the strongest wins an attach.
    pub const fn strength(&self) -> u8 {
        match self {
            Self::External(_) => 4,
            Self::Email => 3,
            Self::Phone => 2,
            Self::AnonymousId => 1,
        }
    }
}

/// A normalized identifier.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct Identifier {
    kind: IdentifierKind,
    value: String,
}

// Identifier values are personal data: never print them through Debug.
impl fmt::Debug for Identifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Identifier({}, ..)", self.kind.as_text())
    }
}

impl Identifier {
    /// Normalizes `value` for the kind named by `kind`.
    pub fn parse(kind: &str, value: &str) -> Result<Self, IdentifierError> {
        let kind = IdentifierKind::parse(kind)?;
        let value = match &kind {
            IdentifierKind::Email => normalize_email(value)?,
            IdentifierKind::Phone => normalize_phone(value)?,
            IdentifierKind::AnonymousId | IdentifierKind::External(_) => exact(value)?,
        };
        Ok(Self { kind, value })
    }

    /// Builds an identifier from stored, already normalized parts.
    pub fn from_stored(kind: IdentifierKind, value: String) -> Self {
        Self { kind, value }
    }

    pub fn kind(&self) -> &IdentifierKind {
        &self.kind
    }

    pub fn value(&self) -> &str {
        &self.value
    }
}

/// The keyed hash that stands in for an identifier where its value must not be stored, such
/// as a suppression: the `Identifier`-purpose HMAC of `<kind>:<normalized value>`. The
/// workspace is not part of the message, so a hash can be reused across workspaces.
pub fn identifier_hash(key: &HmacKey, identifier: &Identifier) -> [u8; 32] {
    key.digest(format!("{}:{}", identifier.kind.as_text(), identifier.value).as_bytes())
}

fn normalize_email(value: &str) -> Result<String, IdentifierError> {
    crate::Email::parse(value)
        .map(|email| email.as_str().to_owned())
        .map_err(|_| IdentifierError::InvalidEmail)
}

fn normalize_phone(value: &str) -> Result<String, IdentifierError> {
    let stripped: String = value
        .trim()
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '.' | '(' | ')'))
        .collect();
    let international = match stripped.strip_prefix("00") {
        Some(rest) => format!("+{rest}"),
        None => stripped,
    };
    let digits = international
        .strip_prefix('+')
        .ok_or(IdentifierError::InvalidPhone)?;
    if (8..=15).contains(&digits.len()) && digits.bytes().all(|byte| byte.is_ascii_digit()) {
        Ok(international)
    } else {
        Err(IdentifierError::InvalidPhone)
    }
}

fn exact(value: &str) -> Result<String, IdentifierError> {
    let valid = !value.is_empty()
        && value.chars().count() <= MAX_VALUE_CHARS
        && !value.chars().any(char::is_control);
    if valid {
        Ok(value.to_owned())
    } else {
        Err(IdentifierError::InvalidValue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(kind: &str, input: &str) -> Result<String, IdentifierError> {
        Identifier::parse(kind, input).map(|identifier| identifier.value().to_owned())
    }

    fn hash_of(kind: &str, input: &str) -> [u8; 32] {
        let master =
            crate::MasterKey::parse("BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=").unwrap();
        let key = master.hmac_key(crate::HmacPurpose::Identifier);
        identifier_hash(&key, &Identifier::parse(kind, input).unwrap())
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn the_identifier_hash_matches_the_known_answer() {
        assert_eq!(
            hex(&hash_of("email", "person@example.com")),
            "7c325dbca9868e8788b135375bda17ca42f6d51e2380d660b2b16a291dd8c844"
        );
    }

    #[test]
    fn the_identifier_hash_is_taken_after_normalization_and_depends_on_the_kind() {
        assert_eq!(
            hash_of("email", "  Person@Example.COM "),
            hash_of("email", "person@example.com")
        );
        assert_eq!(
            hash_of("phone", "+45 12 34 56 78"),
            hash_of("phone", "0045-12345678")
        );
        assert_ne!(
            hash_of("anonymous_id", "person@example.com"),
            hash_of("external:crm", "person@example.com")
        );
    }

    #[test]
    fn email_is_trimmed_and_lowercased() {
        assert_eq!(
            value("email", "  Ada@Example.COM "),
            Ok("ada@example.com".into())
        );
        assert_eq!(
            value("email", "not-an-email"),
            Err(IdentifierError::InvalidEmail)
        );
    }

    #[test]
    fn phone_drops_punctuation_and_turns_a_leading_00_into_plus() {
        assert_eq!(value("phone", "+45 12 34-56.78"), Ok("+4512345678".into()));
        assert_eq!(value("phone", "(0045) 1234 5678"), Ok("+4512345678".into()));
    }

    #[test]
    fn phone_needs_an_international_number_of_8_to_15_digits() {
        assert_eq!(
            value("phone", "12345678"),
            Err(IdentifierError::InvalidPhone)
        );
        assert_eq!(
            value("phone", "+1234567"),
            Err(IdentifierError::InvalidPhone)
        );
        assert_eq!(
            value("phone", "+1234567890123456"),
            Err(IdentifierError::InvalidPhone)
        );
        assert_eq!(
            value("phone", "+12345678a"),
            Err(IdentifierError::InvalidPhone)
        );
        assert!(value("phone", "+123456789012345").is_ok());
    }

    #[test]
    fn anonymous_and_external_values_are_kept_exact() {
        assert_eq!(value("anonymous_id", " Abc "), Ok(" Abc ".into()));
        assert_eq!(value("external:crm", "A-1"), Ok("A-1".into()));
        assert_eq!(
            value("anonymous_id", ""),
            Err(IdentifierError::InvalidValue)
        );
        assert_eq!(
            value("anonymous_id", "a\u{7}b"),
            Err(IdentifierError::InvalidValue)
        );
        assert_eq!(
            value("anonymous_id", &"x".repeat(257)),
            Err(IdentifierError::InvalidValue)
        );
        assert!(value("anonymous_id", &"x".repeat(256)).is_ok());
    }

    #[test]
    fn kinds_are_parsed_and_namespaces_checked() {
        assert_eq!(IdentifierKind::parse("email"), Ok(IdentifierKind::Email));
        assert_eq!(
            IdentifierKind::parse("external:crm.v2-a_b"),
            Ok(IdentifierKind::External("crm.v2-a_b".into()))
        );
        assert_eq!(
            IdentifierKind::parse("external:"),
            Err(IdentifierError::InvalidNamespace)
        );
        assert_eq!(
            IdentifierKind::parse("external:CRM"),
            Err(IdentifierError::InvalidNamespace)
        );
        assert_eq!(
            IdentifierKind::parse(&format!("external:{}", "a".repeat(65))),
            Err(IdentifierError::InvalidNamespace)
        );
        assert_eq!(
            IdentifierKind::parse("fax"),
            Err(IdentifierError::UnknownKind)
        );
    }

    #[test]
    fn strength_orders_external_email_phone_anonymous() {
        let external = IdentifierKind::External("x".into());
        assert!(external.strength() > IdentifierKind::Email.strength());
        assert!(IdentifierKind::Email.strength() > IdentifierKind::Phone.strength());
        assert!(IdentifierKind::Phone.strength() > IdentifierKind::AnonymousId.strength());
        assert_eq!(external.as_text(), "external:x");
    }

    #[test]
    fn debug_hides_the_value() {
        let identifier = Identifier::parse("email", "ada@example.com").unwrap();
        assert!(!format!("{identifier:?}").contains("ada"));
    }
}
