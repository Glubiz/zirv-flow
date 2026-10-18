//! What a publish reads besides the flow: whether the workspace can sign a webhook call, and
//! how healthy the sources are that feed what the flow reads (ruling 5c679ecb: record and warn,
//! never pause).

use hashbrown::HashMap;
use serde_json::Value;
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{Freshness, WorkspaceId};

use crate::{StoreError, segment_data_age, stream_freshness};

/// The stored purpose of the per-workspace secret a webhook action signs with.
const WEBHOOK_SIGNING_PURPOSE: &str = "webhook_signing";

/// Whether the workspace has a webhook signing secret. The secret itself is created and rotated
/// by the webhook action's own endpoint; a publish only needs to know it exists.
pub async fn has_webhook_signing_secret(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<bool, StoreError> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM encrypted_secrets WHERE workspace_id = $1 AND purpose = $2)",
    )
    .bind(workspace.as_uuid())
    .bind(WEBHOOK_SIGNING_PURPOSE)
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)
}

/// A source that feeds what a flow reads and is not fresh.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnhealthyInput {
    pub source_id: Uuid,
    pub name: String,
    /// `Stale` or `Failing`, the worst state among the source's enabled streams.
    pub state: Freshness,
    /// When its oldest feeding stream last succeeded; none when one never has.
    pub last_success_at: Option<OffsetDateTime>,
}

/// The sources that feed `deps` (the merged dependency summary of everything the flow reads) and
/// are stale or failing at `now`, by source name. A source is as unhealthy as its worst enabled
/// stream.
pub async fn unhealthy_inputs(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    deps: &Value,
    now: OffsetDateTime,
) -> Result<Vec<UnhealthyInput>, StoreError> {
    let age = segment_data_age(connection, workspace, deps).await?;
    if age.sources.is_empty() {
        return Ok(Vec::new());
    }
    let mut worst: HashMap<Uuid, Freshness> = HashMap::new();
    for stream in stream_freshness(&mut *connection, Some(workspace), None, now).await? {
        let held = worst.entry(stream.source_id).or_insert(Freshness::Fresh);
        *held = (*held).max(stream.state);
    }
    Ok(age
        .sources
        .into_iter()
        .filter_map(|source| {
            let state = worst
                .get(&source.source_id)
                .copied()
                .unwrap_or(Freshness::Fresh);
            (state != Freshness::Fresh).then_some(UnhealthyInput {
                source_id: source.source_id,
                name: source.name,
                state,
                last_success_at: source.last_success_at,
            })
        })
        .collect())
}
