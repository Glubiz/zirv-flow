//! The shape of a validated manifest v1. Built only by [`parse`](super::parse); every value in
//! it has been checked against the limits of the format.

use serde_json::Value;
use url::Url;
use zirv_flow_core::credentials::{CredentialKind, CredentialSchema};
use zirv_flow_core::mapping::Mapping;

use crate::runtime::ClientAuth;
use crate::sources::FieldType;

/// A manifest: one HTTP API, how to authenticate to it and the streams it offers.
#[derive(Clone, Debug)]
pub struct Manifest {
    pub base_url: Url,
    pub auth: Auth,
    /// The shape of the credential the API needs, when the manifest declares one (`auth.credential`).
    pub credential: Option<CredentialSpec>,
    pub rate_limit: Option<RateLimitSpec>,
    /// Static headers sent with every request.
    pub headers: Vec<(String, String)>,
    pub streams: Vec<StreamSpec>,
}

/// How requests prove who is asking. The secrets are never in the manifest: each type names the
/// credential fields it reads (see [`declared_credential_fields`](super::declared_credential_fields)).
#[derive(Clone, Debug)]
pub enum Auth {
    None,
    ApiKey {
        location: KeyLocation,
        name: String,
    },
    Basic,
    Bearer,
    OAuth2ClientCredentials {
        token_url: Url,
        scopes: Vec<String>,
        client_auth: ClientAuth,
    },
    OAuth2AuthorizationCode {
        authorize_url: Url,
        token_url: Url,
        scopes: Vec<String>,
        client_auth: ClientAuth,
    },
}

/// The credential schema a manifest names: a built-in kind by name, or its own fields.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CredentialSpec {
    Kind(CredentialKind),
    Schema(CredentialSchema),
}

impl CredentialSpec {
    /// The schema the credential form is rendered from.
    pub fn schema(&self) -> CredentialSchema {
        match self {
            Self::Kind(kind) => kind.schema(),
            Self::Schema(schema) => schema.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyLocation {
    Header,
    Query,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RateLimitSpec {
    pub requests: u32,
    pub per_seconds: u32,
}

#[derive(Clone, Debug)]
pub struct StreamSpec {
    pub name: String,
    /// Starts with `/`, relative to the base URL's path.
    pub path: String,
    /// Static query parameters; values are text, numbers or booleans rendered as text.
    pub query: Vec<(String, String)>,
    /// A JSON pointer to the array of records; empty for a top-level array.
    pub records_path: String,
    pub pagination: PaginationSpec,
    pub fields: Vec<FieldSpec>,
    pub primary_key: Vec<String>,
    pub incremental: Option<IncrementalSpec>,
    pub delete_signal: Option<DeleteSignal>,
    /// The mapping the stream is seeded with when discovery first sees it.
    pub mapping: Option<Mapping>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FieldSpec {
    pub name: String,
    pub field_type: FieldType,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PaginationSpec {
    None,
    Cursor {
        param: String,
        next: String,
    },
    Offset {
        param: String,
        limit_param: String,
        limit: u32,
    },
    Page {
        param: String,
        first: u32,
        size: Option<(String, u32)>,
    },
    LinkHeader,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorType {
    Timestamp,
    Integer,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IncrementalSpec {
    /// A top-level field of the stream.
    pub cursor_field: String,
    /// The query parameter the checkpoint is sent as.
    pub request_param: String,
    pub cursor_type: CursorType,
}

/// A record whose value at `path` equals `equals` is reported as deleted.
#[derive(Clone, Debug, PartialEq)]
pub struct DeleteSignal {
    /// A dotted path in the `core::mapping` syntax.
    pub path: String,
    /// A string, number or boolean.
    pub equals: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pagination_spec_compares_by_its_parameters() {
        let page = PaginationSpec::Page {
            param: "page".to_owned(),
            first: 1,
            size: None,
        };

        assert_eq!(page.clone(), page);
        assert_ne!(page, PaginationSpec::LinkHeader);
    }
}
