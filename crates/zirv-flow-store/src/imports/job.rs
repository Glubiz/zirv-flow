//! The jobs that work an import.

use serde_json::json;
use sqlx::PgExecutor;
use uuid::Uuid;
use zirv_flow_core::{TraceContext, WorkspaceId};

use crate::{NewJob, StoreError, enqueue};

/// The job kind that runs an import.
pub const RUN_JOB_KIND: &str = "import.run";

/// The job kind that dry-runs an import.
pub const DRY_RUN_JOB_KIND: &str = "import.dry_run";

/// Enqueues the run or the dry run of an import, in the caller's transaction so the job
/// commits with the state change that started it. One job per kind and import holds the key,
/// so an import never has two runs. False when the key is held already.
pub async fn enqueue_import_job<'e>(
    executor: impl PgExecutor<'e>,
    kind: &'static str,
    workspace: WorkspaceId,
    import: Uuid,
    trace: &TraceContext,
) -> Result<bool, StoreError> {
    let mut job = NewJob::new(kind, trace.clone());
    job.workspace_id = Some(workspace);
    job.payload = json!({ "import_id": import });
    job.unique_key = Some(format!("{kind}:{import}"));
    enqueue(executor, &job).await
}
