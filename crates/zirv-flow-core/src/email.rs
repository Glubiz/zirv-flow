use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

use crate::ValidationError;

const MAX_LENGTH: usize = 254;

/// A trimmed, lower-cased e-mail address with a basic syntax check.
#[derive(Clone, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Email(String);

impl Email {
    pub fn parse(value: &str) -> Result<Self, ValidationError> {
        let invalid = || ValidationError::new("email", "is not a valid email address");
        let value = value.trim().to_lowercase();
        if value.is_empty() || value.chars().count() > MAX_LENGTH {
            return Err(invalid());
        }
        if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(invalid());
        }
        let mut parts = value.split('@');
        match (parts.next(), parts.next(), parts.next()) {
            (Some(local), Some(domain), None) if !local.is_empty() && !domain.is_empty() => {
                Ok(Self(value))
            }
            _ => Err(invalid()),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// Addresses are personal data: never print them through Debug.
impl fmt::Debug for Email {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Email(..)")
    }
}

impl fmt::Display for Email {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Email {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_is_trimmed_and_lower_cased() {
        let email = Email::parse("  Jane.Doe@Example.COM \n").unwrap();

        assert_eq!(email.as_str(), "jane.doe@example.com");
    }

    #[test]
    fn malformed_addresses_are_rejected() {
        for bad in [
            "",
            "   ",
            "plain",
            "@example.com",
            "jane@",
            "a@b@c",
            "ja ne@example.com",
            "jane@exa\nmple.com",
        ] {
            let error = Email::parse(bad).unwrap_err();
            assert_eq!(error.field(), "email", "{bad:?}");
        }
    }

    #[test]
    fn the_length_limit_is_254_characters() {
        let domain = "@example.com";
        let ok = format!("{}{domain}", "a".repeat(MAX_LENGTH - domain.len()));
        let too_long = format!("{}{domain}", "a".repeat(MAX_LENGTH - domain.len() + 1));

        assert!(Email::parse(&ok).is_ok());
        assert!(Email::parse(&too_long).is_err());
    }

    #[test]
    fn debug_output_hides_the_address() {
        let email = Email::parse("jane@example.com").unwrap();

        assert!(!format!("{email:?}").contains("jane"));
    }

    #[test]
    fn deserialising_applies_the_same_rules() {
        let email: Email = serde_json::from_str("\" A@B.C \"").unwrap();

        assert_eq!(email.as_str(), "a@b.c");
        assert!(serde_json::from_str::<Email>("\"nope\"").is_err());
    }
}
