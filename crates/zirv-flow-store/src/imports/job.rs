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

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use zirv_flow_core::{Cause, Slug, WorkspaceName};

    use super::*;
    use crate::create_workspace;

    #[test]
    fn the_kind_names_are_import_run_and_import_dry_run() {
        assert_eq!(RUN_JOB_KIND, "import.run");
        assert_eq!(DRY_RUN_JOB_KIND, "import.dry_run");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_pass_holds_one_job_per_import_and_kind_with_the_workspace_and_the_trace(
        pool: PgPool,
    ) {
        crate::decision_testing::checked(pool, |pool| async move {
            let name = WorkspaceName::parse("one").unwrap();
            let slug = Slug::parse("one").unwrap();
            let ws = create_workspace(&pool, &name, &slug).await.unwrap().id;
            let import = Uuid::now_v7();
            let trace = TraceContext::root(Cause::api("req"));

            let first = enqueue_import_job(&pool, RUN_JOB_KIND, ws, import, &trace)
                .await
                .unwrap();
            let again = enqueue_import_job(&pool, RUN_JOB_KIND, ws, import, &trace)
                .await
                .unwrap();
            let dry = enqueue_import_job(&pool, DRY_RUN_JOB_KIND, ws, import, &trace)
                .await
                .unwrap();

            assert!(first && !again && dry);
            let jobs: Vec<(String, Option<Uuid>, serde_json::Value, Option<String>)> =
                sqlx::query_as(
                    "SELECT kind, workspace_id, payload, unique_key FROM jobs ORDER BY kind",
                )
                .fetch_all(&pool)
                .await
                .unwrap();
            assert_eq!(jobs.len(), 2);
            assert_eq!(jobs[1].0, "import.run");
            assert_eq!(jobs[1].1, Some(ws.as_uuid()));
            assert_eq!(jobs[1].2, serde_json::json!({ "import_id": import }));
            assert_eq!(jobs[1].3, Some(format!("import.run:{import}")));
        })
        .await;
    }
}
