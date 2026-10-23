//! Domain types and validation for zirv-flow.
//!
//! This crate is pure: no database, HTTP or network dependencies.

pub mod consent;
pub mod contacts;
pub mod credentials;
pub mod csv_file;
pub mod egress;
mod email;
pub mod events;
pub mod flow;
pub mod freshness;
mod ids;
pub mod mapping;
pub mod password;
pub mod people;
mod role;
pub mod schedule;
mod scope;
mod secrets;
pub mod segment;
pub mod sending;
pub mod signature;
pub mod token;
mod trace;
mod validation;
mod workspace;

pub use email::Email;
pub use freshness::{Freshness, FreshnessInput, Threshold, effective_threshold, freshness};
pub use ids::{ApiKeyId, SecretId, UserId, WorkspaceId};
pub use role::Role;
pub use scope::Scope;
pub use secrets::{
    HmacKey, HmacPurpose, HmacRoot, Keyring, KeyringError, MasterKey, MasterKeyError, Sealed,
    SecretBytes, SecretError, SecretPurpose, SecretString, deployment_key_aad, open, rewrap, seal,
    secret_aad,
};
pub use trace::{Cause, CauseKind, TraceContext, TraceId};
pub use validation::ValidationError;
pub use workspace::{OrphanPolicy, Slug, WorkspaceName};
