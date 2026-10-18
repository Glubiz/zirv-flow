//! Flows with immutable versions (architecture sections 9.1, 14 and 16.2): a draft that is edited
//! freely, a publish that copies it into the next version under the flow's lock, and the
//! lifecycle of a flow (`draft`, `published`, `paused`, `archived`).
//! This crate never sees the validator: the API validates the draft it read under the lock and
//! passes the same locked flow to the publish.

mod crud;
mod inputs;
mod lifecycle;
mod state;

pub use crud::{
    Flow, FlowError, FlowPatch, FlowVersion, LockedFlow, NewFlow, create_flow, find_flow,
    find_flow_version, list_flow_versions, list_flows, lock_flow, update_flow_draft,
};
pub use inputs::{UnhealthyInput, has_webhook_signing_secret, unhealthy_inputs};
pub use lifecycle::{archive_flow, pause_flow, publish_flow, resume_flow};
pub use state::{FlowAction, FlowState};

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::{Value, json};
    use sqlx::PgPool;
    use uuid::Uuid;
    use zirv_flow_core::{Cause, Slug, TraceContext, WorkspaceId, WorkspaceName};

    use super::*;
    use crate::{begin, commit, create_workspace};

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    fn trace() -> TraceContext {
        TraceContext::root(Cause::api("t"))
    }

    fn graph(marker: &str) -> Value {
        json!({"v": 1, "nodes": [{"id": marker, "type": "exit"}], "edges": []})
    }

    fn settings() -> Value {
        json!({"trigger": {"type": "manual"}, "reentry": {"mode": "once"}})
    }

    async fn create(pool: &PgPool, ws: WorkspaceId, name: &str) -> Flow {
        let new = NewFlow {
            id: Uuid::now_v7(),
            name: name.to_owned(),
            graph: graph("draft"),
            settings: settings(),
        };
        let mut connection = pool.acquire().await.unwrap();
        create_flow(&mut connection, ws, &new, None).await.unwrap()
    }

    /// Locks, publishes and commits, as the API does after its validation.
    async fn publish(
        pool: &PgPool,
        ws: WorkspaceId,
        id: Uuid,
    ) -> Result<(Flow, FlowVersion), FlowError> {
        let mut tx = begin(pool).await?;
        let locked = lock_flow(&mut tx, ws, id).await?;
        let published = publish_flow(&mut tx, ws, &locked, None, &trace()).await?;
        commit(tx).await?;
        Ok(published)
    }

    async fn edit(pool: &PgPool, ws: WorkspaceId, id: Uuid, marker: &str) -> Flow {
        let mut connection = pool.acquire().await.unwrap();
        let patch = FlowPatch {
            graph: Some(graph(marker)),
            ..FlowPatch::default()
        };
        update_flow_draft(&mut connection, ws, id, &patch)
            .await
            .unwrap()
    }

    async fn decisions(pool: &PgPool, ws: WorkspaceId, action: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM decision_records WHERE workspace_id = $1 AND action = $2",
        )
        .bind(ws.as_uuid())
        .bind(action)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn version_rows(pool: &PgPool, flow: Uuid) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM flow_versions WHERE flow_id = $1")
            .bind(flow)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn lifecycle(
        pool: &PgPool,
        ws: WorkspaceId,
        id: Uuid,
        action: FlowAction,
    ) -> Result<Flow, FlowError> {
        let mut tx = begin(pool).await?;
        let flow = match action {
            FlowAction::Pause => pause_flow(&mut tx, ws, id, &trace()).await?,
            FlowAction::Resume => resume_flow(&mut tx, ws, id, &trace()).await?,
            FlowAction::Archive => archive_flow(&mut tx, ws, id, &trace()).await?,
            FlowAction::Publish => unreachable!("a publish is not a transition"),
        };
        commit(tx).await?;
        Ok(flow)
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_new_flow_is_a_draft_with_no_version(pool: PgPool) {
        let ws = workspace(&pool, "one").await;

        let flow = create(&pool, ws, "Welcome").await;

        assert_eq!(flow.state, FlowState::Draft);
        assert_eq!(flow.current_version, None);
        assert_eq!(flow.draft_graph, graph("draft"));
        assert!(
            list_flow_versions(&pool, ws, flow.id)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(find_flow(&pool, ws, flow.id).await.unwrap(), Some(flow));
    }

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
    async fn deleting_the_flow_or_the_workspace_removes_the_versions(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let gone = create(&pool, one, "A").await;
        publish(&pool, one, gone.id).await.unwrap();
        let kept = create(&pool, one, "B").await;
        publish(&pool, one, kept.id).await.unwrap();
        let other = create(&pool, two, "C").await;
        publish(&pool, two, other.id).await.unwrap();

        sqlx::query("DELETE FROM flows WHERE id = $1")
            .bind(gone.id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(two.as_uuid())
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(version_rows(&pool, gone.id).await, 0);
        assert_eq!(version_rows(&pool, kept.id).await, 1);
        assert_eq!(version_rows(&pool, other.id).await, 0);
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
        publish_flow(&mut tx, ws, &locked, None, &trace())
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
                let (_, version) = publish_flow(&mut tx, ws, &locked, None, &trace())
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

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_live_name_is_unique_ignoring_case_and_an_archived_name_is_free(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;
        let first = create(&pool, ws, "Welcome").await;
        let new = |name: &str| NewFlow {
            id: Uuid::now_v7(),
            name: name.to_owned(),
            graph: graph("x"),
            settings: settings(),
        };
        let mut connection = pool.acquire().await.unwrap();

        let duplicate = create_flow(&mut connection, ws, &new("WELCOME"), None).await;
        let elsewhere = create_flow(&mut connection, other, &new("Welcome"), None).await;
        drop(connection);
        lifecycle(&pool, ws, first.id, FlowAction::Archive)
            .await
            .unwrap();
        let mut connection = pool.acquire().await.unwrap();
        let reused = create_flow(&mut connection, ws, &new("Welcome"), None).await;

        assert!(duplicate.unwrap_err().is_conflict());
        assert!(elsewhere.is_ok());
        assert!(reused.is_ok());
        let live: Vec<String> = list_flows(&pool, ws, false)
            .await
            .unwrap()
            .into_iter()
            .map(|flow| flow.name)
            .collect();
        let all = list_flows(&pool, ws, true).await.unwrap();
        assert_eq!(live, ["Welcome"]);
        assert_eq!(all.len(), 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_flow_is_scoped_to_its_workspace(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let flow = create(&pool, one, "Welcome").await;
        publish(&pool, one, flow.id).await.unwrap();

        assert!(find_flow(&pool, two, flow.id).await.unwrap().is_none());
        assert!(list_flows(&pool, two, true).await.unwrap().is_empty());
        assert!(
            find_flow_version(&pool, two, flow.id, 1)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            list_flow_versions(&pool, two, flow.id)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            publish(&pool, two, flow.id).await,
            Err(FlowError::NotFound)
        ));
        assert!(matches!(
            lifecycle(&pool, two, flow.id, FlowAction::Archive).await,
            Err(FlowError::NotFound)
        ));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_signing_secret_is_found_only_in_its_own_workspace(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let mut connection = pool.acquire().await.unwrap();
        assert!(
            !has_webhook_signing_secret(&mut connection, one)
                .await
                .unwrap()
        );
        for (workspace, purpose) in [(two, "webhook_signing"), (one, "webhook_secret")] {
            sqlx::query(
                "INSERT INTO encrypted_secrets \
                    (id, workspace_id, purpose, key_id, wrapped_key, nonce, ciphertext) \
                 VALUES ($1, $2, $3, 'k', '\\x00', decode(repeat('00', 24), 'hex'), '\\x00')",
            )
            .bind(Uuid::now_v7())
            .bind(workspace.as_uuid())
            .bind(purpose)
            .execute(&pool)
            .await
            .unwrap();
        }

        assert!(
            !has_webhook_signing_secret(&mut connection, one)
                .await
                .unwrap()
        );
        assert!(
            has_webhook_signing_secret(&mut connection, two)
                .await
                .unwrap()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_source_that_failed_is_reported_for_what_the_flow_reads_and_a_healthy_one_is_not(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let now = time::OffsetDateTime::now_utc();
        let failing = Uuid::now_v7();
        let healthy = Uuid::now_v7();
        let mapping = |trait_key: &str| {
            json!({"columns": [
                {"column": "e", "target": {"type": "identifier", "kind": "email"}},
                {"column": "p", "target": {"type": "trait", "key": trait_key}}
            ]})
        };
        for (source, name, trait_key, failed) in [
            (failing, "CRM", "plan", true),
            (healthy, "Billing", "tier", false),
        ] {
            sqlx::query(
                "INSERT INTO sources (id, workspace_id, name, kind, config, enabled) \
                 VALUES ($1, $2, $3, 'postgres', '{}', true)",
            )
            .bind(source)
            .bind(ws.as_uuid())
            .bind(name)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO streams (id, workspace_id, source_id, name, enabled, sync_mode, \
                    mapping, discovered, enabled_at, last_success_at, last_run_failed) \
                 VALUES ($1, $2, $3, 'contacts', true, 'full_refresh', $4, '{}', $5, $5, $6)",
            )
            .bind(Uuid::now_v7())
            .bind(ws.as_uuid())
            .bind(source)
            .bind(mapping(trait_key))
            .bind(now)
            .bind(failed)
            .execute(&pool)
            .await
            .unwrap();
        }
        let mut connection = pool.acquire().await.unwrap();

        let reads_plan = unhealthy_inputs(&mut connection, ws, &json!({"traits": ["plan"]}), now)
            .await
            .unwrap();
        let reads_tier = unhealthy_inputs(&mut connection, ws, &json!({"traits": ["tier"]}), now)
            .await
            .unwrap();
        let reads_nothing = unhealthy_inputs(&mut connection, ws, &json!({}), now)
            .await
            .unwrap();

        assert_eq!(reads_plan.len(), 1);
        assert_eq!(reads_plan[0].source_id, failing);
        assert_eq!(reads_plan[0].state, zirv_flow_core::Freshness::Failing);
        assert!(reads_tier.is_empty());
        assert!(reads_nothing.is_empty());
    }
}
