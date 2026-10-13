//! The drain's steps for hook rows: load each hook's mapping and the trait catalog once per
//! batch, map a row's payload with the function the preview calls, and apply the result through
//! the profile, event and list paths. Mapping runs here, not at the endpoint, so a mapping edit
//! applies to payloads still queued.
//!
//! Nothing here keeps a payload value outside the apply paths: a refusal holds field names,
//! codes and a masked sample.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;
use zirv_flow_core::mapping::{Mapping, TraitState, map_json, reject_sample};
use zirv_flow_core::{HmacKey, WorkspaceId};

use crate::events::{EventBatch, EventOutcome, EventRecord, apply_event_record};
use crate::ingest::{InboxRow, strongest_identifier};
use crate::lists::{ListSource, MembershipError, add_members};
use crate::people::{ProfileOutcome, ProfileRecord, apply_profile_record, list_trait_definitions};
use crate::{PgConnection, StoreError};

/// The only stream of a hook.
const STREAM: &str = "payload";

/// The reject code of a row whose hook cannot map it any more.
const MAPPING_UNAVAILABLE: &str = "mapping_unavailable";

/// What the drain needs of one hook.
struct HookConfig {
    source: String,
    stream: String,
    /// `None` when the stream is unmapped or its mapping cannot be read.
    mapping: Option<Mapping>,
}

/// The hooks of a batch and the workspace's trait registry, loaded once.
#[derive(Default)]
pub(crate) struct HookConfigs {
    hooks: HashMap<Uuid, HookConfig>,
    catalog: HashMap<String, TraitState>,
    plain: HashSet<String>,
}

impl HookConfigs {
    /// Loads the hooks the batch's webhook rows belong to, and the trait registry when there
    /// are any. A batch without a webhook row reads nothing.
    pub(crate) async fn load(
        connection: &mut PgConnection,
        workspace: WorkspaceId,
        rows: &[InboxRow],
    ) -> Result<Self, StoreError> {
        let mut ids: Vec<Uuid> = rows.iter().filter_map(|row| row.webhook_id).collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            return Ok(Self::default());
        }
        let found: Vec<(Uuid, Uuid, String, Option<Value>)> = sqlx::query_as(
            "SELECT w.id, w.source_id, t.name, t.mapping \
             FROM webhooks w \
             JOIN streams t ON t.workspace_id = w.workspace_id AND t.id = w.stream_id \
             WHERE w.workspace_id = $1 AND w.id = ANY($2)",
        )
        .bind(workspace.as_uuid())
        .bind(&ids)
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        let hooks = found
            .into_iter()
            .map(|(id, source, stream, mapping)| {
                let mapping = mapping.and_then(|value| serde_json::from_value(value).ok());
                (
                    id,
                    HookConfig {
                        source: source.to_string(),
                        stream,
                        mapping,
                    },
                )
            })
            .collect();
        let mut catalog = HashMap::new();
        let mut plain = HashSet::new();
        for definition in list_trait_definitions(&mut *connection, workspace).await? {
            if definition.archived {
                catalog.insert(definition.key, TraitState::Archived);
                continue;
            }
            if !definition.pii {
                plain.insert(definition.key.clone());
            }
            catalog.insert(definition.key, TraitState::Active(definition.trait_type));
        }
        Ok(Self {
            hooks,
            catalog,
            plain,
        })
    }

    /// The source id text and stream name of a hook, for a reject of a row that could not be
    /// mapped.
    pub(crate) fn origin(&self, hook: Uuid) -> Option<(&str, &str)> {
        self.hooks
            .get(&hook)
            .map(|config| (config.source.as_str(), config.stream.as_str()))
    }
}

/// A payload the mapping accepted.
#[derive(Clone, Debug)]
pub(crate) struct HookRecord {
    pub(crate) profile: ProfileRecord,
    pub(crate) event: Option<EventRecord>,
    pub(crate) lists: Vec<Uuid>,
}

/// A payload the mapping refused: codes and a masked sample, never a value.
#[derive(Clone, Debug)]
pub(crate) struct HookRejected {
    pub(crate) source: String,
    pub(crate) stream: String,
    /// `(field, code)` pairs; the field is empty when no one field is at fault.
    pub(crate) reasons: Vec<(String, &'static str)>,
    pub(crate) sample: Value,
}

/// What a hook row maps to.
#[derive(Clone, Debug)]
pub(crate) enum HookMapped {
    Record(Box<HookRecord>),
    Rejected(HookRejected),
}

/// Maps a hook row. `None` is poison: the row is malformed or its hook is gone.
pub(crate) fn map_hook_row(row: &InboxRow, configs: &HookConfigs) -> Option<HookMapped> {
    let config = configs.hooks.get(&row.webhook_id?)?;
    let payload = row.item.get("payload")?;
    payload.as_object()?;
    let signed_at = match row.item.get("signed_at") {
        Some(Value::String(text)) => Some(OffsetDateTime::parse(text, &Rfc3339).ok()?),
        _ => None,
    };
    let Some(mapping) = &config.mapping else {
        return Some(HookMapped::Rejected(HookRejected {
            source: config.source.clone(),
            stream: config.stream.clone(),
            reasons: vec![(String::new(), MAPPING_UNAVAILABLE)],
            sample: json!({"properties": {}, "masked": []}),
        }));
    };
    let sample = || {
        let sample = reject_sample(mapping, payload, |key| configs.plain.contains(key));
        json!({"properties": sample.properties, "masked": sample.masked})
    };
    match map_json(mapping, payload, mapping.root_fields(), &configs.catalog) {
        Err(errors) => Some(HookMapped::Rejected(HookRejected {
            source: config.source.clone(),
            stream: config.stream.clone(),
            reasons: errors
                .into_iter()
                .map(|error| (error.column, error.reason.as_str()))
                .collect(),
            sample: sample(),
        })),
        Ok(mapped) => {
            let Some(record_key) = strongest_identifier(&mapped.identifiers) else {
                return Some(HookMapped::Rejected(HookRejected {
                    source: config.source.clone(),
                    stream: config.stream.clone(),
                    reasons: vec![(String::new(), "missing_identifier")],
                    sample: sample(),
                }));
            };
            let trace = row.trace();
            let event = mapped.event.map(|event| EventRecord {
                source_record_id: None,
                source: config.source.clone(),
                identifiers: mapped.identifiers.clone(),
                name: event.name,
                properties: event.properties,
                timestamp: event.occurred_at,
                received_at: row.received_at,
                message_id: Some(row.message_id.clone()),
                trace: trace.clone(),
            });
            Some(HookMapped::Record(Box::new(HookRecord {
                profile: ProfileRecord {
                    source: config.source.clone(),
                    stream: STREAM.to_owned(),
                    record_key,
                    identifiers: mapped.identifiers,
                    traits: mapped.traits,
                    // A replayed older payload carries an older time and cannot overwrite a
                    // newer trait value.
                    observed_at: signed_at.unwrap_or(row.received_at),
                    trace,
                },
                event,
                lists: mapped.lists,
            })))
        }
    }
}

/// Removes from `record` the traits the hook's ledger row already holds from a payload observed
/// later. The ledger keeps one row per record key and a write replaces it whatever its time, so
/// without this a replayed older payload would overwrite what a newer one set.
async fn drop_superseded_traits(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    record: &mut ProfileRecord,
) -> Result<(), StoreError> {
    if record.traits.is_empty() {
        return Ok(());
    }
    let stored: Option<Value> = sqlx::query_scalar(
        "SELECT traits FROM source_records \n         WHERE workspace_id = $1 AND source = $2 AND stream = $3 AND record_key = $4 \n           AND deleted_at IS NULL",
    )
    .bind(workspace.as_uuid())
    .bind(&record.source)
    .bind(&record.stream)
    .bind(&record.record_key)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let Some(Value::Object(stored)) = stored else {
        return Ok(());
    };
    let observed = record.observed_at;
    record.traits.retain(|key, _| {
        let newer = stored
            .get(key)
            .and_then(|entry| entry.get("at"))
            .and_then(Value::as_str)
            .and_then(|at| OffsetDateTime::parse(at, &Rfc3339).ok())
            .is_some_and(|at| at > observed);
        !newer
    });
    Ok(())
}

/// What applying a hook record did.
pub(crate) struct HookResult {
    pub(crate) profile: ProfileOutcome,
    pub(crate) event: Option<EventOutcome>,
}

/// Applies a hook record: the profile, then the event when the profile is current (written or
/// already so), then the list memberships of the person. A list that no longer exists is an
/// error, so the batch rolls back and the row is isolated singly.
pub(crate) async fn apply_hook_record(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &HmacKey,
    events: &mut EventBatch,
    record: &HookRecord,
) -> Result<HookResult, StoreError> {
    let mut current = record.profile.clone();
    drop_superseded_traits(connection, workspace, &mut current).await?;
    let profile = apply_profile_record(connection, workspace, &current).await?;
    let person = match &profile {
        ProfileOutcome::Applied { person_id, .. } | ProfileOutcome::Unchanged { person_id, .. } => {
            *person_id
        }
        ProfileOutcome::Rejected { .. } => {
            return Ok(HookResult {
                profile,
                event: None,
            });
        }
    };
    let event = match &record.event {
        Some(event) => Some(apply_event_record(connection, workspace, key, events, event).await?),
        None => None,
    };
    let mut lists = record.lists.clone();
    lists.sort_unstable();
    lists.dedup();
    for list in lists {
        match add_members(
            connection,
            workspace,
            list,
            &[person],
            ListSource::Sync,
            None,
            &record.profile.trace,
        )
        .await
        {
            Ok(_) => {}
            Err(MembershipError::Store(error)) => return Err(error),
            Err(MembershipError::NoSuchList | MembershipError::TooMany) => {
                return Err(StoreError::poison());
            }
        }
    }
    Ok(HookResult { profile, event })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;

    use super::super::fixtures::{hook, key, mapping, plan_trait, trace, workspace};
    use super::super::{Webhook, append_webhook, find_for_receive};
    use super::*;
    use crate::{Shard, create_list, drain_batch, drain_batch_singly, load_profile};

    async fn push(
        pool: &PgPool,
        created: &Webhook,
        payload: Value,
        signed_at: Option<OffsetDateTime>,
    ) {
        let receive = find_for_receive(pool, created.id).await.unwrap().unwrap();
        let id = payload["id"].as_str().unwrap_or("none").to_owned();
        let message = format!("wh:{}:{id}", created.id);
        append_webhook(
            pool,
            &receive,
            &trace(),
            &payload,
            signed_at,
            &message,
            false,
        )
        .await
        .unwrap();
    }

    async fn count(pool: &PgPool, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
    }

    fn order(id: &str, email: &str, plan: &str) -> Value {
        json!({"id": id, "email": email, "plan": plan, "subscribed": "true", "order": {"total": 12}})
    }

    async fn the_plan(pool: &PgPool, ws: WorkspaceId) -> Option<Value> {
        let person: Uuid = sqlx::query_scalar("SELECT id FROM people WHERE workspace_id = $1")
            .bind(ws.as_uuid())
            .fetch_one(pool)
            .await
            .unwrap();
        let profile = load_profile(pool, ws, person).await.unwrap().unwrap();
        profile
            .traits
            .into_iter()
            .find(|entry| entry.key == "plan")
            .map(|entry| entry.value)
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_payload_makes_the_person_trait_event_and_list_member_once_and_a_replay_changes_nothing(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        plan_trait(&pool, ws).await;
        let list = create_list(&pool, ws, "buyers", "", None).await.unwrap();
        let created = hook(&pool, ws, "orders", Some(&mapping(&[list.id]))).await;
        let at = Some(datetime!(2026-03-01 10:00 UTC));
        push(&pool, &created, order("o-1", "ada@example.com", "pro"), at).await;

        let outcome = drain_batch(&pool, ws, Shard::ALL, &key(), 100)
            .await
            .unwrap();

        assert_eq!(outcome.rows, 1);
        assert_eq!(outcome.applied.get(&("webhook", "applied")), Some(&1));
        assert_eq!(outcome.applied.get(&("webhook", "accepted")), Some(&1));
        assert_eq!(count(&pool, "SELECT count(*) FROM people").await, 1);
        assert_eq!(count(&pool, "SELECT count(*) FROM events").await, 1);
        assert_eq!(count(&pool, "SELECT count(*) FROM list_members").await, 1);
        assert_eq!(count(&pool, "SELECT count(*) FROM ingest_inbox").await, 0);
        assert_eq!(the_plan(&pool, ws).await, Some(json!("pro")));

        push(&pool, &created, order("o-1", "ada@example.com", "pro"), at).await;
        let again = drain_batch(&pool, ws, Shard::ALL, &key(), 100)
            .await
            .unwrap();

        assert_eq!(again.applied.get(&("webhook", "unchanged")), Some(&1));
        assert_eq!(again.applied.get(&("webhook", "duplicate")), Some(&1));
        assert_eq!(count(&pool, "SELECT count(*) FROM events").await, 1);
        assert_eq!(count(&pool, "SELECT count(*) FROM list_members").await, 1);
        assert_eq!(count(&pool, "SELECT count(*) FROM source_records").await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_replayed_older_payload_never_overwrites_a_newer_trait(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        plan_trait(&pool, ws).await;
        let created = hook(&pool, ws, "orders", Some(&mapping(&[]))).await;
        let newer = Some(datetime!(2026-03-02 10:00 UTC));
        let older = Some(datetime!(2026-03-01 10:00 UTC));
        push(
            &pool,
            &created,
            order("o-2", "ada@example.com", "pro"),
            newer,
        )
        .await;
        drain_batch(&pool, ws, Shard::ALL, &key(), 100)
            .await
            .unwrap();

        push(
            &pool,
            &created,
            order("o-1", "ada@example.com", "free"),
            older,
        )
        .await;
        drain_batch(&pool, ws, Shard::ALL, &key(), 100)
            .await
            .unwrap();

        assert_eq!(the_plan(&pool, ws).await, Some(json!("pro")));
        assert_eq!(count(&pool, "SELECT count(*) FROM events").await, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_payload_the_mapping_refuses_is_one_reject_with_codes_and_a_masked_sample_and_the_rest_applies(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        plan_trait(&pool, ws).await;
        let created = hook(&pool, ws, "orders", Some(&mapping(&[]))).await;
        let bad = json!({"id": "bad", "plan": "pro", "secret": "hunter2"});
        push(&pool, &created, bad, None).await;
        push(
            &pool,
            &created,
            order("o-1", "ada@example.com", "pro"),
            None,
        )
        .await;

        let outcome = drain_batch(&pool, ws, Shard::ALL, &key(), 100)
            .await
            .unwrap();

        assert_eq!(outcome.applied.get(&("webhook", "rejected")), Some(&1));
        assert_eq!(outcome.applied.get(&("webhook", "applied")), Some(&1));
        let rejects: Vec<(String, String, String, Value, Value)> =
            sqlx::query_as("SELECT source, name, reason, reasons, sample FROM ingest_rejects")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rejects.len(), 1);
        assert_eq!(rejects[0].0, created.source_id.to_string());
        assert_eq!(rejects[0].1, "payload");
        assert_eq!(rejects[0].2, "missing_identifier");
        assert!(!format!("{:?}", rejects[0]).contains("hunter2"));
        let tied = count(
            &pool,
            "SELECT count(*) FROM ingest_rejects WHERE sync_run_id IS NOT NULL",
        )
        .await;
        assert_eq!(tied, 0);
        assert_eq!(count(&pool, "SELECT count(*) FROM people").await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_payload_queued_before_the_stream_lost_its_mapping_is_rejected_as_unavailable(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let created = hook(&pool, ws, "orders", Some(&mapping(&[]))).await;
        push(
            &pool,
            &created,
            order("o-1", "ada@example.com", "pro"),
            None,
        )
        .await;
        sqlx::query("UPDATE streams SET mapping = NULL WHERE id = $1")
            .bind(created.stream_id)
            .execute(&pool)
            .await
            .unwrap();

        let outcome = drain_batch(&pool, ws, Shard::ALL, &key(), 100)
            .await
            .unwrap();

        assert_eq!(outcome.applied.get(&("webhook", "rejected")), Some(&1));
        let reason: String = sqlx::query_scalar("SELECT reason FROM ingest_rejects")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(reason, "mapping_unavailable");
        assert_eq!(count(&pool, "SELECT count(*) FROM people").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_poison_hook_row_is_isolated_as_apply_failed_and_the_batch_goes_on(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        plan_trait(&pool, ws).await;
        let created = hook(&pool, ws, "orders", Some(&mapping(&[]))).await;
        sqlx::query(
            "INSERT INTO ingest_inbox \
                (workspace_id, webhook_id, kind, item, message_id, trace_id, affinity) \
             VALUES ($1, $2, 'webhook', $3, 'wh:poison', $4, 0)",
        )
        .bind(ws.as_uuid())
        .bind(created.id)
        .bind(json!({"payload": 5}))
        .bind(Uuid::now_v7())
        .execute(&pool)
        .await
        .unwrap();
        push(
            &pool,
            &created,
            order("o-1", "ada@example.com", "pro"),
            None,
        )
        .await;

        assert!(
            drain_batch(&pool, ws, Shard::ALL, &key(), 100)
                .await
                .is_err()
        );
        let outcome = drain_batch_singly(&pool, ws, Shard::ALL, &key(), 100)
            .await
            .unwrap();

        assert_eq!(outcome.applied.get(&("webhook", "apply_failed")), Some(&1));
        assert_eq!(outcome.applied.get(&("webhook", "applied")), Some(&1));
        let rejected: (String, String) =
            sqlx::query_as("SELECT reason, source FROM ingest_rejects")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(rejected.0, "apply_failed");
        assert_eq!(rejected.1, created.source_id.to_string());
        assert_eq!(count(&pool, "SELECT count(*) FROM ingest_inbox").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_singly_path_applies_profile_event_and_lists_and_rejects_in_a_savepoint(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        plan_trait(&pool, ws).await;
        let list = create_list(&pool, ws, "buyers", "", None).await.unwrap();
        let created = hook(&pool, ws, "orders", Some(&mapping(&[list.id]))).await;
        push(
            &pool,
            &created,
            order("o-1", "ada@example.com", "pro"),
            None,
        )
        .await;
        push(&pool, &created, json!({"id": "bad"}), None).await;

        let outcome = drain_batch_singly(&pool, ws, Shard::ALL, &key(), 100)
            .await
            .unwrap();

        assert_eq!(outcome.applied.get(&("webhook", "applied")), Some(&1));
        assert_eq!(outcome.applied.get(&("webhook", "accepted")), Some(&1));
        assert_eq!(outcome.applied.get(&("webhook", "rejected")), Some(&1));
        assert_eq!(count(&pool, "SELECT count(*) FROM list_members").await, 1);
        assert_eq!(count(&pool, "SELECT count(*) FROM ingest_rejects").await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn two_hooks_with_the_same_dedupe_value_each_keep_their_event(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        plan_trait(&pool, ws).await;
        let first = hook(&pool, ws, "one", Some(&mapping(&[]))).await;
        let second = hook(&pool, ws, "two", Some(&mapping(&[]))).await;
        push(&pool, &first, order("same", "ada@example.com", "pro"), None).await;
        push(
            &pool,
            &second,
            order("same", "ada@example.com", "pro"),
            None,
        )
        .await;

        drain_batch(&pool, ws, Shard::ALL, &key(), 100)
            .await
            .unwrap();

        assert_eq!(count(&pool, "SELECT count(*) FROM events").await, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_without_a_hook_row_reads_no_hook_configuration(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let mut connection = pool.acquire().await.unwrap();

        let configs = HookConfigs::load(&mut connection, ws, &[]).await.unwrap();

        assert!(configs.hooks.is_empty());
        assert!(configs.catalog.is_empty());
    }
}
