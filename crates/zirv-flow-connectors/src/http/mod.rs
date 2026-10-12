//! The guarded HTTP client: the one way connectors make outbound HTTP requests.

mod body;
mod client;
mod error;
mod resolver;

pub use body::{DEFAULT_MAX_BODY_BYTES, read_body};
pub use client::{GuardedClient, HttpLimits};
pub use error::HttpError;
pub use resolver::Lookup;
