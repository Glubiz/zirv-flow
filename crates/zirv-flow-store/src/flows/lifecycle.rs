//! Publishing a flow into an immutable version, and pausing, resuming and archiving it. Each
//! runs in the caller's transaction and records its decision with the change.
//!
//! This crate never sees the validator: the caller locks the flow with [`lock_flow`], validates
//! what it holds, and passes the same [`LockedFlow`] to [`publish_flow`].

use sqlx::PgConnection;
use uuid::Uuid;
use zirv_flow_core::flow::FlowSettings;
use zirv_flow_core::{TraceContext, UserId, WorkspaceId};

use super::crud::{
    Flow, FlowError, FlowRow, FlowVersion, LockedFlow, flow_columns, lock_flow, version_columns,
};
use super::state::{FlowAction, FlowState};
use super::triggers::{enqueue_publish_fan_out, remove_flow_schedule, sync_flow_schedule};
use crate::{Clock, NewDecision, StoreError, append_decision};

/// The state `action` leads to from the locked flow, or the refusal.
fn next_state(locked: &LockedFlow, action: FlowAction) -> Result<FlowState, FlowError> {
    let state = locked.flow.state;
    state
        .after(action)
        .ok_or(FlowError::IllegalTransition { state, action })
}

async fn record(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    flow: Uuid,
    trace: &TraceContext,
    action: &'static str,
    outcome: &'static str,
) -> Result<(), StoreError> {
    append_decision(
        connection,
        &NewDecision {
            workspace_id: workspace,
            trace: trace.clone(),
            action,
            outcome,
            reason: None,
            subject_ids: vec![("flow", flow)],
            job: None,
        },
    )
    .await
}

/// Publishes the locked flow's draft as version n + 1 and makes it current, in the caller's
/// transaction, and records a `flow.publish` decision. A draft becomes `published`; a published
/// or paused flow keeps its state. An archived flow is refused.
pub async fn publish_flow(
    connection: &mut PgConnection,
    clock: &Clock,
    workspace: WorkspaceId,
    locked: &LockedFlow,
    published_by: Option<UserId>,
    trace: &TraceContext,
) -> Result<(Flow, FlowVersion), FlowError> {
    let state = next_state(locked, FlowAction::Publish)?;
    let flow = &locked.flow;
    let version = flow.current_version.unwrap_or(0) + 1;
    let published: FlowVersion = sqlx::query_as(concat!(
        "INSERT INTO flow_versions (workspace_id, flow_id, version, graph, settings, \
            published_by, trace_id) VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING ",
        version_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(flow.id)
    .bind(version)
    .bind(&flow.draft_graph)
    .bind(&flow.draft_settings)
    .bind(published_by.map(|user| user.as_uuid()))
    .bind(trace.trace_id.as_uuid())
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let row: FlowRow = sqlx::query_as(concat!(
        "UPDATE flows SET state = $3, current_version = $4, updated_at = now() \
         WHERE workspace_id = $1 AND id = $2 RETURNING ",
        flow_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(flow.id)
    .bind(state.as_str())
    .bind(version)
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    record(
        connection,
        workspace,
        flow.id,
        trace,
        "flow.publish",
        "published",
    )
    .await?;
    // A draft that does not parse was never validated, and the API refuses to publish one.
    if let Ok(settings) = serde_json::from_value::<FlowSettings>(flow.draft_settings.clone()) {
        sync_flow_schedule(connection, workspace, flow.id, &settings).await?;
        enqueue_publish_fan_out(
            connection, clock, workspace, flow.id, version, &settings, trace,
        )
        .await?;
    }
    Ok((row.into_flow()?, published))
}

async fn transition(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    action: FlowAction,
    trace: &TraceContext,
    decision: (&'static str, &'static str),
) -> Result<Flow, FlowError> {
    let locked = lock_flow(connection, workspace, id).await?;
    let state = next_state(&locked, action)?;
    let row: FlowRow = sqlx::query_as(concat!(
        "UPDATE flows SET state = $3, \
            archived_at = CASE WHEN $3 = 'archived' THEN now() ELSE archived_at END, \
            updated_at = now() WHERE workspace_id = $1 AND id = $2 RETURNING ",
        flow_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(state.as_str())
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    record(connection, workspace, id, trace, decision.0, decision.1).await?;
    if action == FlowAction::Archive {
        remove_flow_schedule(connection, workspace, id).await?;
    }
    Ok(row.into_flow()?)
}

/// Pauses a published flow: no new enrollments until it is resumed. Records `flow.pause`.
pub async fn pause_flow(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    trace: &TraceContext,
) -> Result<Flow, FlowError> {
    transition(
        connection,
        workspace,
        id,
        FlowAction::Pause,
        trace,
        ("flow.pause", "paused"),
    )
    .await
}

/// Resumes a paused flow. Records `flow.resume`.
pub async fn resume_flow(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    trace: &TraceContext,
) -> Result<Flow, FlowError> {
    transition(
        connection,
        workspace,
        id,
        FlowAction::Resume,
        trace,
        ("flow.resume", "resumed"),
    )
    .await
}

/// Archives a flow that is not archived yet. Nothing is deleted: its versions stay readable.
/// Records `flow.archive`.
pub async fn archive_flow(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    trace: &TraceContext,
) -> Result<Flow, FlowError> {
    transition(
        connection,
        workspace,
        id,
        FlowAction::Archive,
        trace,
        ("flow.archive", "archived"),
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::{Value, json};
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;
    use crate::flows::tests_support::*;
    use crate::flows::{
        FlowPatch, find_flow, find_flow_version, list_flow_versions, update_flow_draft,
    };
    use crate::{begin, commit};

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn each_publish_makes_the_next_version_current_and_keeps_the_old_ones(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let flow = create(&pool, ws, "Welcome").await;
        edit(&pool, ws, flow.id, "first").await;

        let (first, one) = publish(&pool, ws, flow.id).await.unwrap();
        edit(&pool, ws, flow.id, "second").await;
        let (second, two) = publish(&pool, ws, flow.id).await.unwrap();

        assert_eq!((first.current_version, one.version), (Some(1), 1));
        assert_eq!((second.current_version, two.version), (Some(2), 2));
        assert_eq!(first.state, FlowState::Published);
        assert_eq!(second.state, FlowState::Published);
        let versions = list_flow_versions(&pool, ws, flow.id).await.unwrap();
        assert_eq!(
            versions.iter().map(|v| v.version).collect::<Vec<_>>(),
            [2, 1]
        );
        let stored = find_flow_version(&pool, ws, flow.id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.graph, graph("first"));
        assert_eq!(two.graph, graph("second"));
        assert_eq!(two.settings, settings());
        assert!(
            find_flow_version(&pool, ws, flow.id, 3)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(decisions(&pool, ws, "flow.publish").await, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_version_is_never_updated_or_deleted_on_its_own(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let flow = create(&pool, ws, "Welcome").await;
        publish(&pool, ws, flow.id).await.unwrap();
        publish(&pool, ws, flow.id).await.unwrap();
        let before = find_flow_version(&pool, ws, flow.id, 1)
            .await
            .unwrap()
            .unwrap();

        let edit_graph = sqlx::query("UPDATE flow_versions SET graph = '{}' WHERE flow_id = $1")
            .bind(flow.id)
            .execute(&pool)
            .await;
        let renumber = sqlx::query("UPDATE flow_versions SET version = 9 WHERE flow_id = $1")
            .bind(flow.id)
            .execute(&pool)
            .await;
        let delete_first =
            sqlx::query("DELETE FROM flow_versions WHERE flow_id = $1 AND version = 1")
                .bind(flow.id)
                .execute(&pool)
                .await;
        let delete_all = sqlx::query("DELETE FROM flow_versions WHERE flow_id = $1")
            .bind(flow.id)
            .execute(&pool)
            .await;
        let dangling = sqlx::query("UPDATE flows SET current_version = 9 WHERE id = $1")
            .bind(flow.id)
            .execute(&pool)
            .await;
        // Editing the draft afterwards leaves every version as it was.
        edit(&pool, ws, flow.id, "later").await;

        assert!(edit_graph.is_err());
        assert!(renumber.is_err());
        assert!(delete_first.is_err());
        assert!(delete_all.is_err());
        assert!(dangling.is_err());
        assert_eq!(version_rows(&pool, flow.id).await, 2);
        let after = find_flow_version(&pool, ws, flow.id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(before, after);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_publish_that_does_not_commit_writes_nothing(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let flow = create(&pool, ws, "Welcome").await;

        // The API validates between the lock and the publish, and drops the transaction when the
        // draft has issues; a transaction that fails after the publish behaves the same.
        let mut tx = begin(&pool).await.unwrap();
        let locked = lock_flow(&mut tx, ws, flow.id).await.unwrap();
        publish_flow(&mut tx, &Clock::default(), ws, &locked, None, &trace())
            .await
            .unwrap();
        drop(tx);
        let mut tx = begin(&pool).await.unwrap();
        lock_flow(&mut tx, ws, flow.id).await.unwrap();
        drop(tx);

        let after = find_flow(&pool, ws, flow.id).await.unwrap().unwrap();
        assert_eq!(after, flow);
        assert_eq!(version_rows(&pool, flow.id).await, 0);
        assert_eq!(decisions(&pool, ws, "flow.publish").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn concurrent_publishes_of_one_flow_run_one_after_the_other(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let flow = create(&pool, ws, "Welcome").await;
        let id = flow.id;

        let slow = {
            let pool = pool.clone();
            async move {
                let mut tx = begin(&pool).await.unwrap();
                let locked = lock_flow(&mut tx, ws, id).await.unwrap();
                // Hold the lock while the other publish arrives and waits.
                tokio::time::sleep(Duration::from_millis(400)).await;
                let (_, version) =
                    publish_flow(&mut tx, &Clock::default(), ws, &locked, None, &trace())
                        .await
                        .unwrap();
                commit(tx).await.unwrap();
                version.version
            }
        };
        let quick = {
            let pool = pool.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                publish(&pool, ws, id).await.unwrap().1.version
            }
        };
        let (first, second) = tokio::join!(slow, quick);

        assert_eq!((first, second), (1, 2));
        let current = find_flow(&pool, ws, id).await.unwrap().unwrap();
        assert_eq!(current.current_version, Some(2));
        assert_eq!(version_rows(&pool, id).await, 2);
        assert_eq!(decisions(&pool, ws, "flow.publish").await, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_lifecycle_moves_through_its_states_and_records_a_decision_for_each(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let flow = create(&pool, ws, "Welcome").await;

        let (published, _) = publish(&pool, ws, flow.id).await.unwrap();
        let paused = lifecycle(&pool, ws, flow.id, FlowAction::Pause)
            .await
            .unwrap();
        let resumed = lifecycle(&pool, ws, flow.id, FlowAction::Resume)
            .await
            .unwrap();
        let archived = lifecycle(&pool, ws, flow.id, FlowAction::Archive)
            .await
            .unwrap();

        assert_eq!(published.state, FlowState::Published);
        assert_eq!(paused.state, FlowState::Paused);
        assert_eq!(resumed.state, FlowState::Published);
        assert_eq!(archived.state, FlowState::Archived);
        assert!(archived.archived_at.is_some());
        assert_eq!(archived.current_version, Some(1));
        for action in ["flow.publish", "flow.pause", "flow.resume", "flow.archive"] {
            assert_eq!(decisions(&pool, ws, action).await, 1, "{action}");
        }
        let subject: Value = sqlx::query_scalar(
            "SELECT subject_ids FROM decision_records WHERE action = 'flow.pause'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(subject, json!({"flow": flow.id.to_string()}));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_publish_keeps_a_paused_flow_paused_and_a_draft_can_be_archived(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let flow = create(&pool, ws, "Welcome").await;
        let never = create(&pool, ws, "Never").await;
        publish(&pool, ws, flow.id).await.unwrap();
        lifecycle(&pool, ws, flow.id, FlowAction::Pause)
            .await
            .unwrap();

        let (still_paused, version) = publish(&pool, ws, flow.id).await.unwrap();
        let archived_draft = lifecycle(&pool, ws, never.id, FlowAction::Archive)
            .await
            .unwrap();

        assert_eq!(still_paused.state, FlowState::Paused);
        assert_eq!(version.version, 2);
        assert_eq!(archived_draft.state, FlowState::Archived);
        assert_eq!(archived_draft.current_version, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn illegal_transitions_are_refused_and_change_nothing(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let draft = create(&pool, ws, "Draft").await;
        let live = create(&pool, ws, "Live").await;
        publish(&pool, ws, live.id).await.unwrap();
        let gone = create(&pool, ws, "Gone").await;
        publish(&pool, ws, gone.id).await.unwrap();
        lifecycle(&pool, ws, gone.id, FlowAction::Archive)
            .await
            .unwrap();

        let pause_draft = lifecycle(&pool, ws, draft.id, FlowAction::Pause).await;
        let resume_draft = lifecycle(&pool, ws, draft.id, FlowAction::Resume).await;
        let resume_live = lifecycle(&pool, ws, live.id, FlowAction::Resume).await;
        let pause_gone = lifecycle(&pool, ws, gone.id, FlowAction::Pause).await;
        let resume_gone = lifecycle(&pool, ws, gone.id, FlowAction::Resume).await;
        let archive_gone = lifecycle(&pool, ws, gone.id, FlowAction::Archive).await;
        let publish_gone = publish(&pool, ws, gone.id).await;
        let edit_gone = {
            let mut connection = pool.acquire().await.unwrap();
            let patch = FlowPatch {
                name: Some("Renamed".into()),
                ..FlowPatch::default()
            };
            update_flow_draft(&mut connection, ws, gone.id, &patch).await
        };
        let missing = lifecycle(&pool, ws, Uuid::now_v7(), FlowAction::Pause).await;

        for (result, state, action) in [
            (pause_draft, FlowState::Draft, FlowAction::Pause),
            (resume_draft, FlowState::Draft, FlowAction::Resume),
            (resume_live, FlowState::Published, FlowAction::Resume),
            (pause_gone, FlowState::Archived, FlowAction::Pause),
            (resume_gone, FlowState::Archived, FlowAction::Resume),
            (archive_gone, FlowState::Archived, FlowAction::Archive),
        ] {
            match result {
                Err(FlowError::IllegalTransition {
                    state: held,
                    action: refused,
                }) => assert_eq!((held, refused), (state, action)),
                other => panic!("expected a refusal of {action} in {state}, got {other:?}"),
            }
        }
        assert!(matches!(
            publish_gone,
            Err(FlowError::IllegalTransition {
                state: FlowState::Archived,
                action: FlowAction::Publish
            })
        ));
        assert!(matches!(edit_gone, Err(FlowError::Archived)));
        assert!(matches!(missing, Err(FlowError::NotFound)));
        assert_eq!(version_rows(&pool, gone.id).await, 1);
        assert_eq!(decisions(&pool, ws, "flow.pause").await, 0);
        assert_eq!(decisions(&pool, ws, "flow.resume").await, 0);
        assert_eq!(decisions(&pool, ws, "flow.archive").await, 1);
        assert_eq!(decisions(&pool, ws, "flow.publish").await, 2);
        let after = find_flow(&pool, ws, draft.id).await.unwrap().unwrap();
        assert_eq!(after.state, FlowState::Draft);
    }
}
