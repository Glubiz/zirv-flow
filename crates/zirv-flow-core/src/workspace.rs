use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

use crate::ValidationError;

const MAX_NAME: usize = 100;
const MAX_SLUG: usize = 63;

/// A workspace display name: 1 to 100 characters after trimming.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WorkspaceName(String);

impl WorkspaceName {
    pub fn parse(value: &str) -> Result<Self, ValidationError> {
        let value = value.trim();
        let length = value.chars().count();
        if length == 0 || length > MAX_NAME || value.chars().any(char::is_control) {
            return Err(ValidationError::new("name", "must be 1 to 100 characters"));
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkspaceName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for WorkspaceName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

/// A URL-safe workspace handle: `[a-z0-9-]`, 1 to 63 characters, no leading or trailing `-`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Slug(String);

impl Slug {
    pub fn parse(value: &str) -> Result<Self, ValidationError> {
        let valid = (1..=MAX_SLUG).contains(&value.len())
            && value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !value.starts_with('-')
            && !value.ends_with('-');
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(ValidationError::new(
                "slug",
                "must be 1 to 63 characters of a-z, 0-9 and -, without a leading or trailing -",
            ))
        }
    }

    /// Derives a slug from a name: lower-cases, turns every run of other characters into
    /// one `-`, and trims `-` from both ends.
    pub fn from_name(name: &WorkspaceName) -> Result<Self, ValidationError> {
        let mut slug = String::new();
        for c in name.as_str().chars() {
            if c.is_ascii_alphanumeric() {
                slug.push(c.to_ascii_lowercase());
            } else if !slug.ends_with('-') {
                slug.push('-');
            }
        }
        let slug = slug.trim_matches('-');
        let slug = &slug[..slug.len().min(MAX_SLUG)];
        Self::parse(slug.trim_end_matches('-'))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Slug {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Slug {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

/// What happens to a person left with no source record, event or explicit consent after a
/// source retracts a record (architecture section 7.4).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OrphanPolicy {
    /// The person stays, with their traits recomputed. The default.
    #[default]
    Keep,
    /// The person is deleted.
    Delete,
}

impl OrphanPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Keep => "keep",
            Self::Delete => "delete",
        }
    }
}

impl std::str::FromStr for OrphanPolicy {
    type Err = ValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "keep" => Ok(Self::Keep),
            "delete" => Ok(Self::Delete),
            _ => Err(ValidationError::new(
                "orphan_policy",
                "must be keep or delete",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_orphan_policy_round_trips_and_defaults_to_keep() {
        for policy in [OrphanPolicy::Keep, OrphanPolicy::Delete] {
            assert_eq!(policy.as_str().parse::<OrphanPolicy>().unwrap(), policy);
        }
        assert_eq!(OrphanPolicy::default(), OrphanPolicy::Keep);
        assert_eq!(
            "purge".parse::<OrphanPolicy>().unwrap_err().field(),
            "orphan_policy"
        );
    }

    #[test]
    fn a_name_is_trimmed_and_bounded() {
        assert_eq!(WorkspaceName::parse("  Acme  ").unwrap().as_str(), "Acme");
        assert!(WorkspaceName::parse("   ").is_err());
        assert!(WorkspaceName::parse(&"a".repeat(100)).is_ok());
        assert!(WorkspaceName::parse(&"a".repeat(101)).is_err());
        assert_eq!(WorkspaceName::parse("").unwrap_err().field(), "name");
    }

    #[test]
    fn a_name_counts_characters_not_bytes() {
        assert!(WorkspaceName::parse(&"é".repeat(100)).is_ok());
    }

    #[test]
    fn slugs_accept_only_the_allowed_alphabet() {
        for good in ["a", "acme", "a-b", "a1-2b", &"a".repeat(63)] {
            assert!(Slug::parse(good).is_ok(), "{good}");
        }
        for bad in ["", "-a", "a-", "A", "a_b", "a b", "é", &"a".repeat(64)] {
            assert_eq!(Slug::parse(bad).unwrap_err().field(), "slug", "{bad}");
        }
    }

    #[test]
    fn a_slug_is_derived_from_the_name() {
        let slug = |name: &str| Slug::from_name(&WorkspaceName::parse(name).unwrap());

        assert_eq!(slug("Acme Corp").unwrap().as_str(), "acme-corp");
        assert_eq!(
            slug("  --Hello,   World!! ").unwrap().as_str(),
            "hello-world"
        );
        assert_eq!(slug("Q3 2026").unwrap().as_str(), "q3-2026");
        assert!(slug("!!!").is_err());
        assert!(slug("é").is_err());
    }

    #[test]
    fn a_derived_slug_is_cut_to_the_limit_without_a_trailing_dash() {
        let name = format!("{} {}", "a".repeat(62), "b".repeat(10));

        let slug = Slug::from_name(&WorkspaceName::parse(&name).unwrap()).unwrap();

        assert_eq!(slug.as_str(), "a".repeat(62));
    }

    #[test]
    fn deserialising_applies_the_same_rules() {
        assert!(serde_json::from_str::<Slug>("\"Bad Slug\"").is_err());
        assert!(serde_json::from_str::<WorkspaceName>("\"\"").is_err());
        assert_eq!(
            serde_json::from_str::<Slug>("\"ok\"").unwrap().as_str(),
            "ok"
        );
    }
}
