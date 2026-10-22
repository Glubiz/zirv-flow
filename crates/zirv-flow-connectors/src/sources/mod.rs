//! Sources: external systems records are read from (architecture 5.3). A source type is one
//! module here plus one line in [`ALL`]. Sources return records and checkpoints and never write
//! the profile store. Every network call goes through the [`SourceContext`].

mod context;
mod credential;
mod error;
#[cfg(feature = "test-support")]
mod fake;
mod file;
mod http;
mod kafka;
mod mysql;
mod postgres;
mod record_key;
mod registry;
mod source;
mod spec;
mod sql;
mod types;
mod validate;
mod webhook;

pub use context::SourceContext;
pub use credential::{Credential, CredentialFormatError};
pub use error::SourceError;
pub use http::{REFRESH_TOKEN_KEY, active_binding, seed_refresh_token};
pub use kafka::{PartitionPosition, Positions, ReadPlan};
pub use registry::Registry;
pub use source::{BoxFuture, ErasedSource, Source};
pub use spec::{Capabilities, FieldKind, FieldSpec, SourceSpec};
pub use types::{
    Batch, Checkpoint, FieldDescriptor, FieldType, Record, Refused, StreamConfig, StreamDescriptor,
    SyncMode,
};
pub use validate::{
    FieldError, StreamSettingsInput, validate_config, validate_credential, validate_stream_settings,
};
pub(crate) use validate::{check_config, check_credential, url_problem_https};
pub use webhook::PAYLOAD_STREAM;

/// The token the fake source's connection test accepts.
#[cfg(feature = "test-support")]
pub use fake::GOOD_TOKEN as FAKE_GOOD_TOKEN;

pub(crate) static ALL: &[&dyn ErasedSource] = &[
    #[cfg(feature = "test-support")]
    &fake::Fake,
    &mysql::Mysql,
    &postgres::Postgres,
    &file::File,
    &http::Http,
    &kafka::Kafka,
    &webhook::Webhook,
];
