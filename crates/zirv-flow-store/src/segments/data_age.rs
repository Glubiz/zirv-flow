//! Data age of a definition (architecture 7.5): how stale the sources that feed it are.
//!
//! A source feeds a definition when one of its enabled streams has a saved mapping that writes
//! a trait, an event or a list the definition reads. The mappings are read from `streams.mapping`
//! and parsed as [`Mapping`]; a definition that references another segment also reads what that
//! segment's current version reads, so the closure over the references is used.

use std::collections::BTreeSet;

use serde_json::Value;
use sqlx::PgConnection;
use sqlx::types::Json;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::mapping::{Mapping, Target};

use super::cycle::references_of;
use crate::StoreError;

/// What a definition reads, from a stored `deps` value.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct Reads {
    traits: BTreeSet<String>,
    events: BTreeSet<String>,
    lists: BTreeSet<Uuid>,
}

impl Reads {
    fn absorb(&mut self, deps: &Value) {
        let strings = |key: &str| -> Vec<String> {
            deps.get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        };
        self.traits.extend(strings("traits"));
        self.events.extend(strings("events"));
        self.lists.extend(
            strings("lists")
                .iter()
                .filter_map(|id| id.parse::<Uuid>().ok()),
        );
    }

    /// Whether `mapping` writes any of what is read.
    fn fed_by(&self, mapping: &Mapping) -> bool {
        let event = mapping
            .event
            .as_ref()
            .is_some_and(|rule| self.events.contains(&rule.name));
        event
            || mapping.columns.iter().any(|rule| match &rule.target {
                Target::Trait { key } => self.traits.contains(key),
                Target::List { list_id } => self.lists.contains(list_id),
                Target::Identifier { .. } | Target::Ignore => false,
            })
    }
}

/// A source that feeds a definition, and when its oldest feeding stream last succeeded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeedingSource {
    pub source_id: Uuid,
    pub name: String,
    /// The oldest `last_success_at` of its feeding streams; `None` when one has never succeeded.
    pub last_success_at: Option<OffsetDateTime>,
}

/// How current a definition's data is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DataAge {
    /// True exactly when no stream feeds the definition: it reads only live data.
    pub live: bool,
    /// The oldest `last_success_at` across the feeding sources. `None` when `live`, and also
    /// when any feeding source has never succeeded (`sources` shows which one).
    pub as_of: Option<OffsetDateTime>,
    pub sources: Vec<FeedingSource>,
}

/// Adds what the current versions of the segments in `deps` read, and theirs in turn.
async fn closure(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    deps: &Value,
) -> Result<Reads, StoreError> {
    let mut reads = Reads::default();
    reads.absorb(deps);
    let mut seen: BTreeSet<Uuid> = BTreeSet::new();
    let mut pending: Vec<Uuid> = references_of(deps);
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        let found: Option<Json<Value>> = sqlx::query_scalar(
            "SELECT v.deps FROM segments s \
             JOIN segment_versions v ON v.segment_id = s.id AND v.version = s.current_version \
             WHERE s.workspace_id = $1 AND s.id = $2",
        )
        .bind(workspace.as_uuid())
        .bind(id)
        .fetch_optional(&mut *connection)
        .await
        .map_err(StoreError::bounded)?;
        if let Some(Json(found)) = found {
            reads.absorb(&found);
            pending.extend(references_of(&found));
        }
    }
    Ok(reads)
}

/// The data age of a definition whose stored `deps` are given. Reads on `connection`, so it
/// belongs in the preview's snapshot.
pub async fn segment_data_age(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    deps: &Value,
) -> Result<DataAge, StoreError> {
    let reads = closure(connection, workspace, deps).await?;
    let streams: Vec<(Uuid, String, Json<Value>, Option<OffsetDateTime>)> = sqlx::query_as(
        "SELECT so.id, so.name, st.mapping, st.last_success_at FROM streams st \
         JOIN sources so ON so.workspace_id = st.workspace_id AND so.id = st.source_id \
         WHERE st.workspace_id = $1 AND st.enabled AND so.enabled AND st.mapping IS NOT NULL \
         ORDER BY lower(so.name), so.id",
    )
    .bind(workspace.as_uuid())
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::bounded)?;

    let mut sources: Vec<FeedingSource> = Vec::new();
    for (source_id, name, Json(mapping), last_success_at) in streams {
        let Ok(mapping) = serde_json::from_value::<Mapping>(mapping) else {
            continue;
        };
        if !reads.fed_by(&mapping) {
            continue;
        }
        match sources.iter_mut().find(|held| held.source_id == source_id) {
            Some(held) => {
                held.last_success_at = match (held.last_success_at, last_success_at) {
                    (Some(left), Some(right)) => Some(left.min(right)),
                    _ => None,
                };
            }
            None => sources.push(FeedingSource {
                source_id,
                name,
                last_success_at,
            }),
        }
    }
    let live = sources.is_empty();
    // One source that never succeeded makes the age unknown, not the age of the others.
    let as_of = if sources
        .iter()
        .any(|source| source.last_success_at.is_none())
    {
        None
    } else {
        sources
            .iter()
            .filter_map(|source| source.last_success_at)
            .min()
    };
    Ok(DataAge {
        live,
        as_of,
        sources,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;
    use crate::create_workspace;

    fn mapping(value: Value) -> Mapping {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn a_mapping_feeds_a_definition_that_reads_a_trait_an_event_or_a_list_it_writes() {
        let list = Uuid::from_u128(5);
        let writes = mapping(json!({
            "columns": [
                {"column": "e", "target": {"type": "identifier", "kind": "email"}},
                {"column": "p", "target": {"type": "trait", "key": "plan"}},
                {"column": "l", "target": {"type": "list", "list_id": list.to_string()}}
            ],
            "event": {"name": "order"}
        }));
        let mut reads = Reads::default();

        assert!(!reads.fed_by(&writes));
        reads.absorb(&json!({"traits": ["plan"]}));
        assert!(reads.fed_by(&writes));
        let mut reads = Reads::default();
        reads.absorb(&json!({"events": ["order"]}));
        assert!(reads.fed_by(&writes));
        let mut reads = Reads::default();
        reads.absorb(&json!({"lists": [list.to_string()]}));
        assert!(reads.fed_by(&writes));
        let mut reads = Reads::default();
        reads.absorb(&json!({"traits": ["email", "other"], "events": ["refund"]}));
        assert!(!reads.fed_by(&writes));
    }

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    async fn source(pool: &PgPool, ws: WorkspaceId, name: &str, enabled: bool) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO sources (id, workspace_id, kind, name, config, enabled) \
             VALUES ($1, $2, 'csv', $3, '{}', $4)",
        )
        .bind(id)
        .bind(ws.as_uuid())
        .bind(name)
        .bind(enabled)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    async fn stream(
        pool: &PgPool,
        ws: WorkspaceId,
        source: Uuid,
        name: &str,
        mapping: Option<Value>,
        enabled: bool,
        last_success_at: Option<OffsetDateTime>,
    ) {
        sqlx::query(
            "INSERT INTO streams (id, workspace_id, source_id, name, enabled, sync_mode, mapping, \
                discovered, enabled_at, last_success_at) \
             VALUES ($1, $2, $3, $4, $5, 'full_refresh', $6, '{}', \
                CASE WHEN $5 THEN now() END, $7)",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .bind(source)
        .bind(name)
        .bind(enabled)
        .bind(mapping)
        .bind(last_success_at)
        .execute(pool)
        .await
        .unwrap();
    }

    fn plan_mapping() -> Value {
        json!({"columns": [{"column": "p", "target": {"type": "trait", "key": "plan"}}]})
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_age_is_the_oldest_success_of_the_streams_whose_mapping_feeds_the_definition(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;
        let old = datetime!(2026-01-01 00:00 UTC);
        let new = datetime!(2026-02-01 00:00 UTC);
        let crm = source(&pool, ws, "CRM", true).await;
        let shop = source(&pool, ws, "Shop", true).await;
        let off = source(&pool, ws, "Off", false).await;
        let foreign = source(&pool, other, "Foreign", true).await;
        stream(
            &pool,
            ws,
            crm,
            "contacts",
            Some(plan_mapping()),
            true,
            Some(new),
        )
        .await;
        stream(
            &pool,
            ws,
            shop,
            "customers",
            Some(plan_mapping()),
            true,
            Some(old),
        )
        .await;
        stream(&pool, ws, shop, "unmapped", None, true, Some(old)).await;
        stream(
            &pool,
            ws,
            off,
            "contacts",
            Some(plan_mapping()),
            true,
            Some(old),
        )
        .await;
        stream(
            &pool,
            other,
            foreign,
            "contacts",
            Some(plan_mapping()),
            true,
            Some(old),
        )
        .await;
        let unrelated =
            json!({"columns": [{"column": "p", "target": {"type": "trait", "key": "age"}}]});
        stream(&pool, ws, crm, "other", Some(unrelated), true, Some(old)).await;
        let mut connection = pool.acquire().await.unwrap();

        let age = segment_data_age(&mut connection, ws, &json!({"traits": ["plan"]}))
            .await
            .unwrap();
        let live = segment_data_age(&mut connection, ws, &json!({"traits": ["nothing"]}))
            .await
            .unwrap();

        assert_eq!(age.as_of, Some(old));
        let found: Vec<(&str, Option<OffsetDateTime>)> = age
            .sources
            .iter()
            .map(|source| (source.name.as_str(), source.last_success_at))
            .collect();
        assert_eq!(found, [("CRM", Some(new)), ("Shop", Some(old))]);
        assert_eq!(
            live,
            DataAge {
                live: true,
                as_of: None,
                sources: Vec::new()
            }
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_feeding_stream_that_never_succeeded_is_listed_without_a_time(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let crm = source(&pool, ws, "CRM", true).await;
        stream(&pool, ws, crm, "contacts", Some(plan_mapping()), true, None).await;
        let mut connection = pool.acquire().await.unwrap();

        let age = segment_data_age(&mut connection, ws, &json!({"traits": ["plan"]}))
            .await
            .unwrap();

        assert!(!age.live);
        assert_eq!(age.as_of, None);
        assert_eq!(age.sources.len(), 1);
        assert_eq!(age.sources[0].last_success_at, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_never_synced_feeder_keeps_the_age_unknown_beside_a_synced_one(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let synced = source(&pool, ws, "Synced", true).await;
        let fresh = source(&pool, ws, "Fresh", true).await;
        let at = datetime!(2026-02-01 00:00 UTC);
        stream(
            &pool,
            ws,
            synced,
            "contacts",
            Some(plan_mapping()),
            true,
            Some(at),
        )
        .await;
        stream(
            &pool,
            ws,
            fresh,
            "contacts",
            Some(plan_mapping()),
            true,
            None,
        )
        .await;
        let mut connection = pool.acquire().await.unwrap();

        let age = segment_data_age(&mut connection, ws, &json!({"traits": ["plan"]}))
            .await
            .unwrap();

        assert!(!age.live);
        assert_eq!(age.as_of, None);
        let found: Vec<(&str, Option<OffsetDateTime>)> = age
            .sources
            .iter()
            .map(|source| (source.name.as_str(), source.last_success_at))
            .collect();
        assert_eq!(found, [("Fresh", None), ("Synced", Some(at))]);
    }
}
