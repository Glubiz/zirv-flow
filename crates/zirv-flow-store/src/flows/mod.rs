//! Flows with immutable versions (architecture sections 9.1, 14 and 16.2): a draft that is edited
//! freely, a publish that copies it into the next version under the flow's lock, and the
//! lifecycle of a flow (`draft`, `published`, `paused`, `archived`).
//! This crate never sees the validator: the API validates the draft it read under the lock and
//! passes the same locked flow to the publish.

mod arrivals;
mod control;
mod crud;
mod delivery;
mod effects;
mod enroll;
mod inputs;
mod inspector;
mod lifecycle;
mod runtime;
mod send;
mod signing;
mod state;
mod triggers;

pub(crate) use arrivals::{Arrival, enqueue_arrivals};
pub use arrivals::{StoredEvent, find_event_of_person};
pub use control::{
    ControlError, ExitBatch, LIFECYCLE_BATCH, LIFECYCLE_JOB_KIND, LifecycleAction, LifecycleBatch,
    LifecyclePayload, SETTLE_JOB_KIND, SettleItem, SettlePayload, Signal, enqueue_lifecycle,
    exit_batch, flow_state_for_step, hold_enrollment, release_enrollment, reopen_failed_step,
    resume_batch, resume_failed, segment_is_watched,
};
pub(crate) use control::{enqueue_segment_settles, exit_people};
pub use crud::{
    Flow, FlowError, FlowPatch, FlowVersion, LockedFlow, NewFlow, create_flow, find_flow,
    find_flow_version, list_flow_versions, list_flows, lock_flow, update_flow_draft,
};
pub use delivery::{
    WebhookOutcome, WebhookPerson, WebhookResult, WebhookSubject, lock_webhook_person,
    read_webhook_subject, record_webhook_outcome,
};
pub use effects::{EffectError, FlowTraitWrite, apply_flow_list, apply_flow_trait, flow_source};
pub use enroll::{
    EnrollEntries, EnrollEntry, EnrollError, EnrollOutcome, EnrollRequest, Enrolled,
    MAX_ENROLL_PEOPLE, Occurrence, enroll, enroll_entries, resolve_survivors,
};
pub use inputs::{UnhealthyInput, has_webhook_signing_secret, unhealthy_inputs};
pub use inspector::{
    EnrollmentCursor, EnrollmentFilter, EnrollmentPage, EnrollmentPath, EnrollmentSummary,
    MAX_PATH_DECISIONS, NodeCount, PathEventWait, PathStep, SETTINGS_GOAL_NODE,
    list_flow_enrollments, load_enrollment_path, version_node_counts,
};
pub use lifecycle::{archive_flow, pause_flow, publish_flow, resume_flow};
pub use runtime::{
    Enrollment, EnrollmentStatus, EnrollmentUpdate, NewStepExecution, NewWait, STEP_JOB_KIND,
    StepJobInput, StepJobPayload, StepOutcome, StepRecord, WEBHOOK_JOB_KIND, clear_waits,
    count_in_flight_enrollments, find_enrollment, lock_enrollment, record_steps, register_wait,
    save_enrollment, settle_wait_step, step_zones, wait_filters,
};
pub use send::{
    DELIVERY_UNCONFIRMED, INTERNAL_ERROR, SEND_JOB_KIND, SendOutcome, end_orphaned_messages,
    record_send_outcome,
};
pub use signing::{
    Rotation, SigningKey, SigningKeys, create_signing_key, live_signing_keys, rotate_signing_key,
};
pub use state::{FlowAction, FlowState};
pub(crate) use triggers::enqueue_segment_enrolls;
pub use triggers::{
    AudienceSource, ENROLL_JOB_KIND, EnrollJobPayload, EnrollWork, TriggerEvent, TriggerKind,
    TriggeredFlow, audience_page, enroll_page_key, find_events, lock_audience_page, occurrence_of,
    reconcile_flow_schedules, segment_trigger_flows,
};

#[cfg(test)]
pub(crate) mod tests_support {
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use uuid::Uuid;
    use zirv_flow_core::{Cause, Slug, TraceContext, WorkspaceId, WorkspaceName};

    use super::{
        Flow, FlowAction, FlowError, FlowPatch, FlowVersion, NewFlow, archive_flow, create_flow,
        lock_flow, pause_flow, publish_flow, resume_flow, update_flow_draft,
    };
    use crate::{begin, commit, create_workspace};

    pub(crate) async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    pub(crate) fn trace() -> TraceContext {
        TraceContext::root(Cause::api("t"))
    }

    pub(crate) fn graph(marker: &str) -> Value {
        json!({"v": 1, "nodes": [{"id": marker, "type": "exit"}], "edges": []})
    }

    pub(crate) fn settings() -> Value {
        json!({"trigger": {"type": "manual"}, "reentry": {"mode": "once"}})
    }

    pub(crate) async fn create(pool: &PgPool, ws: WorkspaceId, name: &str) -> Flow {
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
    pub(crate) async fn publish(
        pool: &PgPool,
        ws: WorkspaceId,
        id: Uuid,
    ) -> Result<(Flow, FlowVersion), FlowError> {
        let mut tx = begin(pool).await?;
        let locked = lock_flow(&mut tx, ws, id).await?;
        let published = publish_flow(
            &mut tx,
            &crate::Clock::default(),
            ws,
            &locked,
            None,
            &trace(),
        )
        .await?;
        commit(tx).await?;
        Ok(published)
    }

    pub(crate) async fn edit(pool: &PgPool, ws: WorkspaceId, id: Uuid, marker: &str) -> Flow {
        let mut connection = pool.acquire().await.unwrap();
        let patch = FlowPatch {
            graph: Some(graph(marker)),
            ..FlowPatch::default()
        };
        update_flow_draft(&mut connection, ws, id, &patch)
            .await
            .unwrap()
    }

    /// A published flow's enrollment asleep at an event wait for `person`, at `state_version` 3.
    pub(crate) async fn waiting_enrollment(
        pool: &PgPool,
        ws: WorkspaceId,
        flow: Uuid,
        person: Uuid,
        event: &str,
    ) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO enrollments \n                (id, workspace_id, flow_id, flow_version, person_id, state, node, occurrence_id, \n                 state_version, trace_id, cause_kind, cause_id, wake_at) \n             VALUES ($1, $2, $3, 1, $4, 'waiting', 'draft', $5, 3, $6, 'api', 't', now())",
        )
        .bind(id)
        .bind(ws.as_uuid())
        .bind(flow)
        .bind(person)
        .bind(Uuid::now_v7())
        .bind(trace().trace_id.as_uuid())
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO enrollment_waits \n                (workspace_id, enrollment_id, person_id, event_name, wake_version, timeout_at) \n             VALUES ($1, $2, $3, $4, 3, now())",
        )
        .bind(ws.as_uuid())
        .bind(id)
        .bind(person)
        .bind(event)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    pub(crate) async fn decisions(pool: &PgPool, ws: WorkspaceId, action: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM decision_records WHERE workspace_id = $1 AND action = $2",
        )
        .bind(ws.as_uuid())
        .bind(action)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    pub(crate) async fn version_rows(pool: &PgPool, flow: Uuid) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM flow_versions WHERE flow_id = $1")
            .bind(flow)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    pub(crate) async fn lifecycle(
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
}
