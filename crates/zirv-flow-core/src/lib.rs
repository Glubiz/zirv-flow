//! Domain types and validation for zirv-flow.
//!
//! This crate is pure: no database, HTTP or network dependencies.

mod email;
mod ids;
pub mod password;
mod role;
pub mod schedule;
mod scope;
pub mod token;
mod validation;
mod workspace;

pub use email::Email;
pub use ids::{ApiKeyId, UserId, WorkspaceId};
pub use role::Role;
pub use scope::Scope;
pub use validation::ValidationError;
pub use workspace::{Slug, WorkspaceName};
