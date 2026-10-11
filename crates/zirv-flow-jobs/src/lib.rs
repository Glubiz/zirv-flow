//! Background jobs for zirv-flow: the worker pool, the scheduler leader and the job-kind
//! registry, over the durable queue in `zirv-flow-store` (architecture section 14).

/// Logs a warning to standard error, where the binary reports its own errors.
macro_rules! warn {
    ($($argument:tt)*) => {
        eprintln!("warning: jobs: {}", format_args!($($argument)*))
    };
}

mod context;
mod error;
mod kinds;
mod metrics;
mod registry;
mod scheduler;
mod worker;

pub use context::JobContext;
pub use error::JobError;
pub use registry::{ErasedKind, JobKind, Registry};
pub use scheduler::{LeaderReport, Scheduler, SchedulerConfig, Tick};
pub use worker::{Worker, WorkerConfig};
