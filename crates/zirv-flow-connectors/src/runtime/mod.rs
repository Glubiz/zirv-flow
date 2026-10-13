//! The connector runtime (architecture 7.5): auth, retries, a per-source rate limit and
//! pagination, implemented once on top of the guarded client so no connector reimplements them.
//! A source reaches it only through [`SourceContext`](crate::sources::SourceContext).

mod error;
mod retry;

pub use error::RuntimeError;
pub use retry::RetryPolicy;
