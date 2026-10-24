//! Destinations: external systems messages are delivered to (architecture 5.3 and 10.1). A
//! destination type is one module here plus one line in [`ALL`]. Destinations hold no state and
//! never touch the store; every network call goes through the [`DestinationContext`].
//!
//! The built-in production types are `webhook`, `smtp` and `provider`. [`ALL`] adds the in-memory fake behind the `test-support` feature, the way the sources
//! registry does.

mod context;
mod destination;
mod dkim;
mod error;
#[cfg(feature = "test-support")]
mod fake;
mod provider;
mod registry;
mod retry_after;
mod smtp;
mod spec;
mod validate;
mod webhook;

pub use context::DestinationContext;
pub use destination::{BoxFuture, Delivered, Destination, ErasedDestination, Outbound};
#[cfg(feature = "test-support")]
pub use dkim::verify_signature as verify_dkim_signature;
pub use dkim::{DkimSigner, DkimSignerError};
pub use error::{DestinationError, Disposition, Refusal};
#[cfg(feature = "test-support")]
pub use provider::fake::{
    Accepted as FakeProviderAccepted, FakeProvider, FakeProviderOptions,
    Script as FakeProviderScript, sign_feedback as sign_provider_feedback,
};
pub use provider::{
    Feedback as ProviderFeedback, FeedbackKind as ProviderFeedbackKind,
    FeedbackRead as ProviderFeedbackRead, ProviderRecord, ProviderRecordStatus,
    default_rate as provider_default_rate, domain_status as provider_domain_status,
    feedback_of as provider_feedback_of, feedback_scheme as provider_feedback_scheme,
    read_feedback as provider_read_feedback,
};
pub use registry::Registry;
pub use retry_after::parse_retry_after;
#[cfg(feature = "test-support")]
pub use smtp::{
    Captured as SmtpCaptured, DROP as SMTP_SINK_DROP, SINK_HOST as SMTP_SINK_HOST,
    SINK_PASSWORD as SMTP_SINK_PASSWORD, SINK_USER as SMTP_SINK_USER,
    SinkOptions as SmtpSinkOptions, SinkStage as SmtpSinkStage, SinkTls as SmtpSinkTls, SmtpSink,
};
pub use spec::{Capabilities, DestinationSpec};
pub use validate::{validate_config, validate_credential, validate_credential_names};

/// The token the fake destination's check accepts.
#[cfg(feature = "test-support")]
pub use fake::GOOD_TOKEN as FAKE_GOOD_TOKEN;
/// The calls and the accepted messages the fake destination keeps under a ledger name.
#[cfg(feature = "test-support")]
pub use fake::{FakeSent, ledger_calls as fake_ledger_calls, ledger_sent as fake_ledger_sent};

pub(crate) static ALL: &[&dyn ErasedDestination] = &[
    &webhook::Webhook,
    &smtp::Smtp,
    &provider::ProviderDestination,
    #[cfg(feature = "test-support")]
    &fake::Fake,
];
