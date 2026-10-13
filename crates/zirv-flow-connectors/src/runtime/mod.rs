//! The connector runtime (architecture 7.5): auth, retries, a per-source rate limit and
//! pagination, implemented once on top of the guarded client so no connector reimplements them.
//! A source reaches it only through [`SourceContext`](crate::sources::SourceContext).

mod auth;
mod error;
mod rate_limit;
mod retry;
mod send;
#[cfg(test)]
mod testing;

pub(crate) use auth::TokenSlot;
pub use auth::{AccessToken, ApiKeyPlacement, Auth, ClientAuth};
pub use error::RuntimeError;
pub use rate_limit::{RateLimit, RateLimits};
pub use reqwest::Method;
pub use retry::RetryPolicy;
pub(crate) use send::send;
pub use send::{RequestSpec, RuntimeResponse};
