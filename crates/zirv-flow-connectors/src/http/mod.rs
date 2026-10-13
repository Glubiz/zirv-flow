//! The guarded HTTP client: the one way connectors make outbound HTTP requests.

mod body;
mod client;
mod error;
mod resolver;

pub use body::DEFAULT_MAX_BODY_BYTES;
pub use client::{GuardedClient, GuardedResponse, HttpLimits};
pub use error::HttpError;
pub use resolver::Lookup;
pub(crate) use resolver::{SystemLookup, check_all};
