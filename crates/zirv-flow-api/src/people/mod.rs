//! The people endpoints: the field registry (architecture section 7.1) and identity conflict
//! review (section 7.3).

pub(crate) mod conflicts;
pub(crate) mod registry;

use zirv_flow_core::{Cause, TraceContext};

pub(crate) use registry::{
    CreateTraitRequest, TraitBody, UpdateTraitRequest, create, list, update,
};

/// The trace of the request being handled, or a new one caused by it.
fn request_trace() -> TraceContext {
    crate::trace::current().unwrap_or_else(|| {
        TraceContext::root(Cause::api(crate::request_id::current().unwrap_or_default()))
    })
}
