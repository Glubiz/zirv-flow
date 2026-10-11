//! The event endpoints: the schema registry, the workspace policy and ingest rejects
//! (architecture section 12.1).

pub(crate) mod policy;
pub(crate) mod rejects;
pub(crate) mod schemas;

pub(crate) use policy::{EventPolicyBody, RedactionBody};
pub(crate) use rejects::{IngestRejectBody, IngestRejectsBody, ReasonBody};
pub(crate) use schemas::{
    CreateEventSchemaRequest, EventSchemaBody, EventSchemaDetailBody, PropertyBody,
    SaveDraftRequest, SchemaVersionBody, UpdateEventSchemaRequest,
};
