use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::ValidationError;

/// What an API key may do inside its own workspace.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Only the ingest endpoints.
    Ingest,
    /// GET requests.
    Read,
    /// Editor-level mutations: drafts, previews and test mode.
    Write,
    /// Publisher-level mutations: publishing, pausing and the operator actions.
    Publish,
    /// Members and keys.
    Admin,
    /// Personal values unmasked. Additive: it does not imply `read`.
    Pii,
}

impl Scope {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ingest => "ingest",
            Self::Read => "read",
            Self::Write => "write",
            Self::Publish => "publish",
            Self::Admin => "admin",
            Self::Pii => "pii",
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Scope {
    type Err = ValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "ingest" => Ok(Self::Ingest),
            "read" => Ok(Self::Read),
            "write" => Ok(Self::Write),
            "publish" => Ok(Self::Publish),
            "admin" => Ok(Self::Admin),
            "pii" => Ok(Self::Pii),
            _ => Err(ValidationError::new(
                "scopes",
                "must be ingest, read, write, publish, admin or pii",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_scope_round_trips_through_text() {
        for scope in [
            Scope::Ingest,
            Scope::Read,
            Scope::Write,
            Scope::Publish,
            Scope::Admin,
            Scope::Pii,
        ] {
            assert_eq!(scope.to_string().parse::<Scope>().unwrap(), scope);
        }
    }

    #[test]
    fn unknown_scopes_are_rejected_naming_the_field() {
        let error = "root".parse::<Scope>().unwrap_err();

        assert_eq!(error.field(), "scopes");
        assert!("READ".parse::<Scope>().is_err());
        assert!("".parse::<Scope>().is_err());
        assert!("PUBLISH".parse::<Scope>().is_err());
        assert!("PII".parse::<Scope>().is_err());
    }

    #[test]
    fn scopes_deserialise_from_lower_case_json() {
        let scopes: Vec<Scope> = serde_json::from_str(r#"["read","write"]"#).unwrap();

        assert_eq!(scopes, [Scope::Read, Scope::Write]);
    }
}
