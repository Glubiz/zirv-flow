use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ValidationError;

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident, $field:literal) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// A fresh random id.
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            pub const fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            pub const fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = ValidationError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value)
                    .map(Self)
                    .map_err(|_| ValidationError::new($field, "is not a valid id"))
            }
        }

        impl From<Uuid> for $name {
            fn from(uuid: Uuid) -> Self {
                Self(uuid)
            }
        }
    };
}

id_type!(
    /// The id of a local account.
    UserId,
    "user_id"
);
id_type!(
    /// The id of a workspace.
    WorkspaceId,
    "workspace_id"
);
id_type!(
    /// The id of a stored secret.
    SecretId,
    "secret_id"
);
id_type!(
    /// The id of an uploaded image asset.
    AssetId,
    "asset_id"
);
id_type!(
    /// The id of an API key; part of the key's public prefix.
    ApiKeyId,
    "api_key_id"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_through_text() {
        let id = UserId::new();

        assert_eq!(id.to_string().parse::<UserId>().unwrap(), id);
        assert_eq!(UserId::from_uuid(id.as_uuid()), id);
    }

    #[test]
    fn a_malformed_id_names_its_field() {
        let error = "nope".parse::<WorkspaceId>().unwrap_err();

        assert_eq!(error.field(), "workspace_id");
    }

    #[test]
    fn ids_serialise_as_plain_uuid_strings() {
        let id = ApiKeyId::new();

        let json = serde_json::to_string(&id).unwrap();

        assert_eq!(json, format!("\"{id}\""));
        assert_eq!(serde_json::from_str::<ApiKeyId>(&json).unwrap(), id);
    }

    #[test]
    fn fresh_ids_differ() {
        assert_ne!(WorkspaceId::new(), WorkspaceId::new());
    }
}
