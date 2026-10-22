//! Destinations: external systems messages are delivered to (architecture 5.3 and 10.1). A
//! destination type is one module here plus one line in [`ALL`]. Destinations hold no state and
//! never touch the store; every network call goes through the [`DestinationContext`].
//!
//! The built-in `webhook` destination is the one production type so far (SMTP arrives with its own
//! issue). [`ALL`] adds the in-memory fake behind the `test-support` feature, the way the sources
//! registry does.

mod context;
mod destination;
mod error;
#[cfg(feature = "test-support")]
mod fake;
mod registry;
mod retry_after;
mod spec;
mod validate;
mod webhook;

pub use context::DestinationContext;
pub use destination::{BoxFuture, Delivered, Destination, ErasedDestination, Outbound};
pub use error::{DestinationError, Disposition};
pub use registry::Registry;
pub use retry_after::parse_retry_after;
pub use spec::{Capabilities, DestinationSpec};
pub use validate::{validate_config, validate_credential};

/// The token the fake destination's check accepts.
#[cfg(feature = "test-support")]
pub use fake::GOOD_TOKEN as FAKE_GOOD_TOKEN;

pub(crate) static ALL: &[&dyn ErasedDestination] = &[
    &webhook::Webhook,
    #[cfg(feature = "test-support")]
    &fake::Fake,
];
