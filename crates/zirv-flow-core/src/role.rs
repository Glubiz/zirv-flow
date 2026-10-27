use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::ValidationError;

/// A user's role inside one workspace. The instance owner is a flag on the user, not a role.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    Publisher,
    Editor,
    Viewer,
}

/// What a request asks of the caller. Each role allows its own capability and every lower one.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Capability {
    /// Read the workspace.
    Read,
    /// Change drafts and other working data.
    Edit,
    /// Publish, pause or archive, and the operator actions on running work.
    Publish,
    /// Sources, destinations, credentials, keys, members and governance.
    Admin,
}

impl Role {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Publisher => "publisher",
            Self::Editor => "editor",
            Self::Viewer => "viewer",
        }
    }

    /// Whether the role holds `capability`: Admin holds all four, Publisher the first three,
    /// Editor `Read` and `Edit`, Viewer only `Read`.
    pub const fn allows(self, capability: Capability) -> bool {
        let level = match self {
            Self::Admin => 3,
            Self::Publisher => 2,
            Self::Editor => 1,
            Self::Viewer => 0,
        };
        let needed = match capability {
            Capability::Admin => 3,
            Capability::Publish => 2,
            Capability::Edit => 1,
            Capability::Read => 0,
        };
        level >= needed
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
            "publisher" => Ok(Self::Publisher),
            "editor" => Ok(Self::Editor),
            "viewer" => Ok(Self::Viewer),
            _ => Err(ValidationError::new(
                "role",
                "must be admin, publisher, editor or viewer",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROLES: [Role; 4] = [Role::Admin, Role::Publisher, Role::Editor, Role::Viewer];

    #[test]
    fn roles_parse_and_display_in_lower_case() {
        for role in ROLES {
            assert_eq!(role.to_string().parse::<Role>().unwrap(), role);
        }
        assert_eq!(Role::Admin.as_str(), "admin");
        assert_eq!(Role::Publisher.as_str(), "publisher");
        assert_eq!(Role::Editor.as_str(), "editor");
        assert_eq!(Role::Viewer.as_str(), "viewer");
    }

    #[test]
    fn unknown_or_differently_cased_roles_are_rejected() {
        assert!("owner".parse::<Role>().is_err());
        assert!("member".parse::<Role>().is_err());
        assert!("Admin".parse::<Role>().is_err());
        assert!("".parse::<Role>().is_err());
    }

    #[test]
    fn roles_serialise_in_lower_case() {
        assert_eq!(
            serde_json::to_string(&Role::Publisher).unwrap(),
            "\"publisher\""
        );
        assert_eq!(
            serde_json::from_str::<Role>("\"viewer\"").unwrap(),
            Role::Viewer
        );
    }

    #[test]
    fn the_capability_lattice_holds_for_every_role() {
        use Capability::{Admin, Edit, Publish, Read};
        let table = [
            (Role::Admin, [true, true, true, true]),
            (Role::Publisher, [true, true, true, false]),
            (Role::Editor, [true, true, false, false]),
            (Role::Viewer, [true, false, false, false]),
        ];
        for (role, expected) in table {
            for (capability, allowed) in [Read, Edit, Publish, Admin].into_iter().zip(expected) {
                assert_eq!(role.allows(capability), allowed, "{role} {capability:?}");
            }
        }
    }
}
