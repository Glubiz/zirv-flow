//! The flow engine's pure core (architecture sections 5.1 and 9.2 to 9.4): [`step`] advances one
//! enrollment through a validated [`FlowGraph`](zirv_flow_core::flow::FlowGraph) and returns a
//! [`Transition`] for the caller to commit. No I/O: time and zones come from a [`StepContext`],
//! branch conditions are evaluated by the caller and handed back as a [`StepInput`].

mod error;
mod hash;
mod state;
mod step;
mod time;

pub use error::EngineError;
pub use hash::{effect_key, split_bucket, step_key, wake_key};
pub use state::{
    ArrivedEvent, Effect, EffectKind, EnrollmentState, ExitReason, Failure, Outcome, Status,
    StepContext, StepInput, Transition, Visit, WaitChange, Wake,
};
pub use step::step;
pub use time::resolve_local;
