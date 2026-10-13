//! The connector runtime (architecture 7.5): auth, retries, a per-source rate limit and
//! pagination, implemented once on top of the guarded client so no connector reimplements them.
//! A source reaches it only through [`SourceContext`](crate::sources::SourceContext).

mod auth;
mod error;
mod paginate;
mod rate_limit;
mod retry;
mod send;
#[cfg(test)]
pub(crate) mod testing;

pub use auth::{
    AccessToken, ApiKeyPlacement, Auth, ClientAuth, CodeExchange, RefreshFuture, TokenStore,
    exchange_code,
};
pub(crate) use auth::{TokenSlot, binding_of};
pub use error::RuntimeError;
pub use paginate::{Page, Pager, Pagination, paginate};
pub use rate_limit::{RateLimit, RateLimits};
pub use reqwest::Method;
pub use retry::RetryPolicy;
pub(crate) use send::send;
pub use send::{RequestSpec, RuntimeResponse};
