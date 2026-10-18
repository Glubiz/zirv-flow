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

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;
    use crate::flows::tests_support::*;

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
