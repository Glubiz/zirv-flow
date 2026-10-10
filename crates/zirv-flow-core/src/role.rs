use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::ValidationError;

/// A user's role inside one workspace. The instance owner is a flag on the user, not a role.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    Member,
}

impl Role {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Member => "member",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = ValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "admin" => Ok(Self::Admin),
            "member" => Ok(Self::Member),
            _ => Err(ValidationError::new("role", "must be admin or member")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_parse_and_display_in_lower_case() {
        for role in [Role::Admin, Role::Member] {
            assert_eq!(role.to_string().parse::<Role>().unwrap(), role);
        }
        assert_eq!(Role::Admin.as_str(), "admin");
    }

    #[test]
    fn unknown_or_differently_cased_roles_are_rejected() {
        assert!("owner".parse::<Role>().is_err());
        assert!("Admin".parse::<Role>().is_err());
    }

    #[test]
    fn roles_serialise_in_lower_case() {
        assert_eq!(serde_json::to_string(&Role::Member).unwrap(), "\"member\"");
    }
}
