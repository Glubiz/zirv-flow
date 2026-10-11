//! Domain types and validation for zirv-flow.
//!
//! This crate is pure: no database, HTTP or network dependencies.

mod email;
mod ids;
pub mod password;
pub mod people;
mod role;
pub mod schedule;
mod scope;
mod secrets;
pub mod token;
mod trace;
mod validation;
mod workspace;

pub use email::Email;
pub use ids::{ApiKeyId, UserId, WorkspaceId};
pub use role::Role;
pub use scope::Scope;
pub use secrets::{HmacKey, HmacPurpose, MasterKey, MasterKeyError};
pub use trace::{Cause, CauseKind, TraceContext, TraceId};
pub use validation::ValidationError;
pub use workspace::{Slug, WorkspaceName};
