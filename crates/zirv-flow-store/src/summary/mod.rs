//! The workspace summary behind the Home figures: aggregate counts over a window, with no person
//! data. A scheduled job fills a daily rollup ([`rollup`]) and the endpoint reads only its rows
//! ([`read`]), so a request costs the window's days and not the data behind them.

mod read;
mod rollup;

pub use read::{FlowFigures, WorkspaceSummary, workspace_summary};
pub use rollup::{COMMIT_MARGIN, refresh_workspace_summary, summary_workspaces};

/// Rows and helpers the tests of both halves seed with.
#[cfg(test)]
pub(super) mod support {
    use serde_json::json;
    use sqlx::PgPool;
    use time::OffsetDateTime;
    use uuid::Uuid;
    use zirv_flow_core::WorkspaceId;

    use crate::flows::tests_support::{create, publish};
    use crate::flows::{FlowPatch, update_flow_draft};

    pub(crate) async fn person(pool: &PgPool, ws: WorkspaceId, created_at: OffsetDateTime) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id, created_at) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(ws.as_uuid())
            .bind(created_at)
            .execute(pool)
            .await
            .unwrap();
        id
    }

    /// An event whose id is a UUIDv7 minted at `received_at`, as ingest mints it.
    pub(crate) async fn event(
        pool: &PgPool,
        ws: WorkspaceId,
        person: Uuid,
        received_at: OffsetDateTime,
    ) {
        let stamp = uuid::Timestamp::from_unix(
            uuid::NoContext,
            received_at.unix_timestamp() as u64,
            received_at.nanosecond(),
        );
        let id = Uuid::new_v7(stamp);
        sqlx::query(
            "INSERT INTO events (id, workspace_id, person_id, name, occurred_at, received_at, \
                                 source, trace_id, validation_state) \
             VALUES ($1, $2, $3, 'order', $4, $4, 'api', $1, 'valid')",
        )
        .bind(id)
        .bind(ws.as_uuid())
        .bind(person)
        .bind(received_at)
        .execute(pool)
        .await
        .unwrap();
    }

    pub(crate) async fn message(
        pool: &PgPool,
        ws: WorkspaceId,
        person: Uuid,
        flow: Option<Uuid>,
        status: &str,
        test: bool,
        completed_at: OffsetDateTime,
    ) {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO messages (id, workspace_id, person_id, enrollment_id, flow_id, \
                                   flow_version, node_id, template_id, destination_id, status, \
                                   reason, test, created_at, completed_at) \
             VALUES ($1, $2, $3, $1, $4, 1, 'send', $1, $1, $5, \
                     CASE WHEN $5 = 'sent' THEN NULL ELSE 'x' END, $6, $7, $7)",
        )
        .bind(id)
        .bind(ws.as_uuid())
        .bind(person)
        .bind(flow)
        .bind(status)
        .bind(test)
        .bind(completed_at)
        .execute(pool)
        .await
        .unwrap();
    }

    pub(crate) async fn enrollment(
        pool: &PgPool,
        ws: WorkspaceId,
        flow: Uuid,
        person: Uuid,
        state: &str,
    ) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO enrollments \
                (id, workspace_id, flow_id, flow_version, person_id, state, node, occurrence_id, \
                 trace_id, cause_kind, cause_id, error, failed_node, ended_at) \
             VALUES ($1, $2, $3, 1, $4, $5, 'draft', $1, $1, 'api', 't', \
                     CASE WHEN $5 = 'failed' THEN 'boom' END, \
                     CASE WHEN $5 = 'failed' THEN 'draft' END, \
                     CASE WHEN $5 IN ('completed', 'exited') THEN now() END)",
        )
        .bind(id)
        .bind(ws.as_uuid())
        .bind(flow)
        .bind(person)
        .bind(state)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    pub(crate) async fn step(
        pool: &PgPool,
        ws: WorkspaceId,
        enrollment: Uuid,
        node: &str,
        started_at: OffsetDateTime,
    ) {
        sqlx::query(
            "INSERT INTO step_executions (id, workspace_id, enrollment_id, node_id, outcome, \
                                          started_at) \
             VALUES ($1, $2, $3, $4, 'done', $5)",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .bind(enrollment)
        .bind(node)
        .bind(started_at)
        .execute(pool)
        .await
        .unwrap();
    }

    /// A published flow; "Live" has a `goal` node named `win` in version 1.
    pub(crate) async fn published(pool: &PgPool, ws: WorkspaceId, name: &str) -> Uuid {
        let flow = create(pool, ws, name).await.id;
        if name == "Live" {
            let patch = FlowPatch {
                graph: Some(json!({"v": 1, "nodes": [{"id": "win", "type": "goal"}], "edges": []})),
                ..FlowPatch::default()
            };
            let mut connection = pool.acquire().await.unwrap();
            update_flow_draft(&mut connection, ws, flow, &patch)
                .await
                .unwrap();
        }
        publish(pool, ws, flow).await.unwrap();
        flow
    }
}
