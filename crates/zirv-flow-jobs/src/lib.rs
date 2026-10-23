//! Background jobs for zirv-flow: the worker pool, the scheduler leader and the job-kind
//! registry, over the durable queue in `zirv-flow-store` (architecture section 14).

mod context;
mod delivery;
mod error;
mod kinds;
mod metrics;
mod registry;
mod scheduler;
#[cfg(test)]
mod segment_compare;
mod worker;

pub use context::JobContext;
pub use delivery::rate::RateGates;
pub use error::JobError;
pub use registry::{ErasedKind, JobKind, Registry};
pub use scheduler::{
    DEFAULT_SENDING_DOMAIN_RECHECK, LeaderReport, Scheduler, SchedulerConfig, Tick,
};
pub use worker::{Worker, WorkerConfig};
/// The DNS resolver a worker reads sending-domain records through, and the real one the
/// composition root builds.
pub use zirv_flow_connectors::dns::{DnsResolver, SystemResolver};
