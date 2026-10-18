//! Publishing a flow into an immutable version, and pausing, resuming and archiving it. Each
//! runs in the caller's transaction and records its decision with the change.
//!
//! This crate never sees the validator: the caller locks the flow with [`lock_flow`], validates
//! what it holds, and passes the same [`LockedFlow`] to [`publish_flow`].

use sqlx::PgConnection;
use uuid::Uuid;
use zirv_flow_core::{TraceContext, UserId, WorkspaceId};

use super::crud::{
    Flow, FlowError, FlowRow, FlowVersion, LockedFlow, flow_columns, lock_flow, version_columns,
};
use super::state::{FlowAction, FlowState};
use crate::{NewDecision, StoreError, append_decision};

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
