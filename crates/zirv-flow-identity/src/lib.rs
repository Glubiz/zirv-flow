//! Outbound calls to an OpenID Connect identity provider: discovery, the signing keys and
//! the authorization-code exchange.

mod error;
#[cfg(any(test, feature = "test-support"))]
pub mod mock;
mod oidc;

pub use error::IdentityError;
pub use oidc::{Authorization, OidcSettings, Provider, SecretString, VerifiedIdentity};
