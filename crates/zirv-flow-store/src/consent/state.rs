//! Consent per person, channel and topic, and its append-only history (architecture
//! sections 6 and 10.1).

use serde_json::Value;
use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::consent::{
    ChangeSource, Channel, ConsentStatus, Dimensions, Read, Record, RecordedConsent, State,
    TOPIC_DIMENSION, TopicPolicy, read, recorded_consent,
};
use zirv_flow_core::decision_actions;
use zirv_flow_core::segment::ChangeClasses;
use zirv_flow_core::{TraceContext, WorkspaceId};

use super::dimensions::{live_dimension, unknown_dimension_entry};
use super::topics::find_topic;
use crate::StoreError;
use crate::changes::{mark_person, request_reevaluation};
use crate::decisions::{NewDecision, append_decision};
use crate::people::current_person;

/// A change to make: the status a person has for a channel, or for one topic of it.
#[derive(Clone, Debug)]
pub struct ConsentChange {
    /// The person written about; a merged person stands for the survivor.
    pub person: Uuid,
    pub channel: Channel,
    /// `None` is the channel as a whole.
    pub topic: Option<Uuid>,
    /// The workspace dimension values the decision is scoped to; a dimension left out means
    /// all its values. The built-in `topic` is `topic`, not a key of this map.
    pub dimensions: Dimensions,
    pub status: ConsentStatus,
    pub source: ChangeSource,
    /// What proves the decision, such as the form, address or text it came from.
    pub evidence: Value,
    /// When the person decided.
    pub occurred_at: OffsetDateTime,
    pub actor: Option<Uuid>,
}

/// What [`set_consent`] did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsentOutcome {
    /// The status was already that: nothing was written.
    Unchanged,
    Changed {
        /// The person the change landed on: the survivor, when the target was merged.
        person_id: Uuid,
        change_id: Uuid,
    },
    /// The change is older than the state it scopes, by (`changed_at`, change id). It was
    /// appended to the history and left the state as it is.
    Superseded,
    NoSuchPerson,
    /// The topic is not in the workspace, or belongs to another channel.
    NoSuchTopic,
    /// A dimension of the scope is not a live one of the workspace.
    NoSuchDimension,
    /// The dimension is live, but a value of the scope is not a live value of it.
    NoSuchDimensionValue,
}

/// One current state of a person, with the topic it is about.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct ConsentEntry {
    pub channel: String,
    pub topic_id: Option<Uuid>,
    pub topic_key: Option<String>,
    pub topic_name: Option<String>,
    /// The dimension values the state is scoped to: a JSON object of key to value key.
    pub dimensions: Value,
    pub status: String,
    pub explicit: bool,
    pub changed_at: OffsetDateTime,
}

/// One row of a person's consent history.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct ConsentHistoryEntry {
    pub id: Uuid,
    pub channel: String,
    pub topic_id: Option<Uuid>,
    pub topic_key: Option<String>,
    /// The dimension values the change is scoped to: a JSON object of key to value key.
    pub dimensions: Value,
    pub from_status: Option<String>,
    pub to_status: String,
    pub source: String,
    pub evidence: Value,
    pub occurred_at: OffsetDateTime,
    pub actor_user_id: Option<Uuid>,
}

/// How many times [`set_consent`] follows a person that is merged away while it looks.
const MERGE_HOPS: usize = 3;

/// Records a consent decision in the caller's transaction. The person is followed through
/// `merged_into`. A write of the status already held writes nothing (no history row, no
/// decision) unless some overlapping record of a different status lies between the held row and
/// the change, as [`reaffirm_matters`] says. Otherwise a history row is appended and a
/// `consent.change` decision records the source code and the ids, and the state is upserted when
/// the change is newer than it; an older change returns [`ConsentOutcome::Superseded`].
pub async fn set_consent(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    change: &ConsentChange,
    trace: &TraceContext,
) -> Result<ConsentOutcome, StoreError> {
    if let Some(topic) = change.topic {
        match find_topic(&mut *connection, workspace, topic).await? {
            Some(found) if found.channel == change.channel => {}
            _ => return Ok(ConsentOutcome::NoSuchTopic),
        }
    }
    if let Some((dimension, _)) =
        unknown_dimension_entry(&mut *connection, workspace, &change.dimensions).await?
    {
        return Ok(
            if live_dimension(connection, workspace, &dimension).await? {
                ConsentOutcome::NoSuchDimensionValue
            } else {
                ConsentOutcome::NoSuchDimension
            },
        );
    }
    let Some(person) = lock_current_person(connection, workspace, change.person).await? else {
        return Ok(ConsentOutcome::NoSuchPerson);
    };
    let held_row: Option<(String, OffsetDateTime, Option<Uuid>)> = sqlx::query_as(
        "SELECT status, changed_at, last_change_id FROM consent \
         WHERE workspace_id = $1 AND person_id = $2 \
           AND channel = $3 AND topic_id IS NOT DISTINCT FROM $4 \
           AND dimensions_key = consent_dimensions_canonical($5)",
    )
    .bind(workspace.as_uuid())
    .bind(person)
    .bind(change.channel.as_str())
    .bind(change.topic)
    .bind(dimensions_json(&change.dimensions))
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let held = held_row.as_ref().map(|(status, _, _)| status.clone());
    let change_id = Uuid::now_v7();
    if let Some((status, at, last)) = &held_row
        && status == change.status.as_str()
        && !reaffirm_matters(
            connection,
            workspace,
            person,
            change,
            change_id,
            (*at, *last),
        )
        .await?
    {
        return Ok(ConsentOutcome::Unchanged);
    }
    let (change_id, applied) = write_state(
        connection,
        workspace,
        person,
        &StateWrite {
            change_id,
            channel: change.channel,
            topic: change.topic,
            dimensions: &change.dimensions,
            from: held.as_deref(),
            state: State {
                status: change.status,
                changed_at: change.occurred_at,
            },
            source: change.source,
            evidence: &change.evidence,
            occurred_at: change.occurred_at,
            actor: change.actor,
            only_if_newer: true,
        },
        trace,
    )
    .await?;
    if !applied {
        return Ok(ConsentOutcome::Superseded);
    }
    request_reevaluation(connection, workspace).await?;
    Ok(ConsentOutcome::Changed {
        person_id: person,
        change_id,
    })
}

/// Whether re-affirming the held status changes what the read rule answers: some other record
/// R of the person and channel overlaps the change scope, has a different status, and sits
/// strictly between the held row and the incoming change by (`changed_at`, change id), the
/// incoming change's pair being (`occurred_at`, `incoming`). Scopes overlap when the topics are
/// equal or either has none, and every dimension key both maps name has the same value; a key
/// missing from a map means all values. Runs under the person lock.
async fn reaffirm_matters(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
    change: &ConsentChange,
    incoming: Uuid,
    held: (OffsetDateTime, Option<Uuid>),
) -> Result<bool, StoreError> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM consent c \
            WHERE c.workspace_id = $1 AND c.person_id = $2 AND c.channel = $3 \
              AND ($4::uuid IS NULL OR c.topic_id IS NULL OR c.topic_id = $4) \
              AND NOT EXISTS (SELECT 1 FROM jsonb_each_text(c.dimensions) e \
                              WHERE $5::jsonb -> e.key IS NOT NULL \
                                AND ($5::jsonb ->> e.key) <> e.value) \
              AND c.status <> $8 \
              AND (c.changed_at, COALESCE(c.last_change_id, '00000000-0000-0000-0000-000000000000'::uuid)) \
                  > ($6, COALESCE($7, '00000000-0000-0000-0000-000000000000'::uuid)) \
              AND (c.changed_at, COALESCE(c.last_change_id, '00000000-0000-0000-0000-000000000000'::uuid)) \
                  < ($9, $10))",
    )
    .bind(workspace.as_uuid())
    .bind(person)
    .bind(change.channel.as_str())
    .bind(change.topic)
    .bind(dimensions_json(&change.dimensions))
    .bind(held.0)
    .bind(held.1)
    .bind(change.status.as_str())
    .bind(change.occurred_at)
    .bind(incoming)
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)
}

/// The JSON object stored for a dimension map.
pub(super) fn dimensions_json(dimensions: &Dimensions) -> Value {
    Value::Object(
        dimensions
            .iter()
            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
            .collect(),
    )
}

/// The dimension map a stored JSON object holds. Anything that is not a string value is
/// dropped; the column's writers only store strings.
pub fn dimensions_of(stored: &Value) -> Dimensions {
    stored
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
        .collect()
}

/// Locks the person that stands for `person` until the transaction ends, so two writes to
/// one person take turns and a merge cannot move the person's rows underneath this one.
async fn lock_current_person(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
) -> Result<Option<Uuid>, StoreError> {
    let mut target = person;
    for _ in 0..MERGE_HOPS {
        let Some(current) = current_person(&mut *connection, workspace, target).await? else {
            return Ok(None);
        };
        let merged_into: Option<Option<Uuid>> = sqlx::query_scalar(
            "SELECT merged_into FROM people WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
        )
        .bind(workspace.as_uuid())
        .bind(current)
        .fetch_optional(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        match merged_into {
            Some(None) => return Ok(Some(current)),
            Some(Some(survivor)) => target = survivor,
            None => return Ok(None),
        }
    }
    Err(StoreError::retryable_race())
}

/// The state and history row to write for one person, channel and topic.
pub(super) struct StateWrite<'a> {
    /// The id of the history row, which becomes the state row's `last_change_id`.
    pub(super) change_id: Uuid,
    pub(super) channel: Channel,
    pub(super) topic: Option<Uuid>,
    pub(super) dimensions: &'a Dimensions,
    pub(super) from: Option<&'a str>,
    pub(super) state: State,
    pub(super) source: ChangeSource,
    pub(super) evidence: &'a Value,
    pub(super) occurred_at: OffsetDateTime,
    pub(super) actor: Option<Uuid>,
    /// Whether the state row changes only when this write is newer by (`changed_at`, change
    /// id) than the row. A merge writes the winner of two states, which can be older.
    pub(super) only_if_newer: bool,
}

/// Appends the history row, upserts the state and records the decision. Returns the id of
/// the history row and whether the state row changed: with `only_if_newer`, a write older than
/// the row stays in the history and leaves the state, and its decision has the outcome
/// `superseded`. The caller holds the person's lock.
pub(super) async fn write_state(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
    write: &StateWrite<'_>,
    trace: &TraceContext,
) -> Result<(Uuid, bool), StoreError> {
    let change_id = write.change_id;
    sqlx::query(
        "INSERT INTO consent_changes \
            (id, workspace_id, person_id, channel, topic_id, dimensions, from_status, to_status, \
             source, evidence, occurred_at, actor_user_id, trace_id) \
         VALUES ($1, $2, $3, $4, $5, $13, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(change_id)
    .bind(workspace.as_uuid())
    .bind(person)
    .bind(write.channel.as_str())
    .bind(write.topic)
    .bind(write.from)
    .bind(write.state.status.as_str())
    .bind(write.source.as_str())
    .bind(write.evidence)
    .bind(write.occurred_at)
    .bind(write.actor)
    .bind(trace.trace_id.as_uuid())
    .bind(dimensions_json(write.dimensions))
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let upsert = sqlx::query(
        "INSERT INTO consent \
            (workspace_id, person_id, channel, topic_id, dimensions, status, explicit, \
             changed_at, last_change_id) \
         VALUES ($1, $2, $3, $4, $9, $5, $6, $7, $8) \
         ON CONFLICT (workspace_id, person_id, channel, topic_id, dimensions_key) DO UPDATE SET \
            status = EXCLUDED.status, explicit = EXCLUDED.explicit, \
            changed_at = EXCLUDED.changed_at, last_change_id = EXCLUDED.last_change_id \
         WHERE NOT $10 OR (consent.changed_at, COALESCE(consent.last_change_id, '00000000-0000-0000-0000-000000000000'::uuid)) \
                          < (EXCLUDED.changed_at, EXCLUDED.last_change_id)",
    )
    .bind(workspace.as_uuid())
    .bind(person)
    .bind(write.channel.as_str())
    .bind(write.topic)
    .bind(write.state.status.as_str())
    .bind(write.state.status.is_explicit())
    .bind(write.state.changed_at)
    .bind(change_id)
    .bind(dimensions_json(write.dimensions))
    .bind(write.only_if_newer)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let applied = upsert.rows_affected() > 0;
    if applied {
        mark_person(
            connection,
            workspace,
            person,
            ChangeClasses::CONSENT,
            trace,
            None,
        )
        .await?;
    }
    append_decision(
        &mut *connection,
        &NewDecision {
            graph: None,
            workspace_id: workspace,
            trace: trace.clone(),
            spec: if applied {
                &decision_actions::CONSENT_CHANGED
            } else {
                &decision_actions::CONSENT_SUPERSEDED
            },
            detail: Some(write.source.as_str()),
            subject_ids: vec![("person", person), ("consent_change", change_id)],
            job: None,
        },
    )
    .await?;
    Ok((change_id, applied))
}

/// The current states of a person, the channel-wide one first and then by topic key. The
/// person is followed through `merged_into`. `None` when the workspace has no such person.
pub async fn consent_for(
    pool: &PgPool,
    workspace: WorkspaceId,
    person: Uuid,
) -> Result<Option<Vec<ConsentEntry>>, StoreError> {
    let Some(person) = current_person(pool, workspace, person).await? else {
        return Ok(None);
    };
    let entries = sqlx::query_as(
        "SELECT c.channel, c.topic_id, t.key AS topic_key, t.name AS topic_name, c.dimensions, \
                c.status, c.explicit, c.changed_at \
         FROM consent c LEFT JOIN topics t ON t.workspace_id = c.workspace_id AND t.id = c.topic_id \
         WHERE c.workspace_id = $1 AND c.person_id = $2 \
         ORDER BY c.channel, t.key NULLS FIRST, c.dimensions_key",
    )
    .bind(workspace.as_uuid())
    .bind(person)
    .fetch_all(pool)
    .await
    .map_err(StoreError::query)?;
    Ok(Some(entries))
}

/// One row of the consent export: a person's state for a channel, topic and dimension scope.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct ConsentExportRow {
    pub person_id: Uuid,
    pub channel: String,
    pub topic_key: Option<String>,
    pub dimensions: Value,
    pub status: String,
    /// Sorts the row within its person, channel and topic; part of the cursor.
    pub dimensions_key: String,
}

impl ConsentExportRow {
    /// Where the page after this row starts.
    pub fn cursor(&self) -> ConsentExportCursor {
        ConsentExportCursor {
            person_id: self.person_id,
            channel: self.channel.clone(),
            topic_key: self.topic_key.clone().unwrap_or_default(),
            dimensions_key: self.dimensions_key.clone(),
        }
    }
}

/// The position after a row of the consent export, in the export's order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConsentExportCursor {
    person_id: Uuid,
    channel: String,
    topic_key: String,
    dimensions_key: String,
}

/// A page of every consent state of the workspace, in the order person, channel, topic key
/// (the channel-wide state first) and canonical dimension scope, after `after`.
pub async fn consent_export_page(
    pool: &PgPool,
    workspace: WorkspaceId,
    after: Option<&ConsentExportCursor>,
    limit: u32,
) -> Result<Vec<ConsentExportRow>, StoreError> {
    let nil = ConsentExportCursor {
        person_id: Uuid::nil(),
        channel: String::new(),
        topic_key: String::new(),
        dimensions_key: String::new(),
    };
    let start = after.unwrap_or(&nil);
    sqlx::query_as(
        "SELECT c.person_id, c.channel, t.key AS topic_key, c.dimensions, c.status,                 c.dimensions_key          FROM consent c          LEFT JOIN topics t ON t.workspace_id = c.workspace_id AND t.id = c.topic_id          WHERE c.workspace_id = $1            AND ($6 OR (c.person_id, c.channel, COALESCE(t.key, ''), c.dimensions_key)                     > ($2, $3, $4, $5))          ORDER BY c.person_id, c.channel, COALESCE(t.key, ''), c.dimensions_key LIMIT $7",
    )
    .bind(workspace.as_uuid())
    .bind(start.person_id)
    .bind(&start.channel)
    .bind(&start.topic_key)
    .bind(&start.dimensions_key)
    .bind(after.is_none())
    .bind(i64::from(limit))
    .fetch_all(pool)
    .await
    .map_err(StoreError::query)
}

/// One state row, as the read rule needs it.
#[derive(FromRow)]
struct RecordRow {
    topic_key: Option<String>,
    dimensions: Value,
    status: String,
    changed_at: OffsetDateTime,
    last_change_id: Option<Uuid>,
}

/// What [`consent_read`] found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadOutcome {
    Read {
        verdict: Read,
        /// The policy whose default applied when no record decided.
        policy: TopicPolicy,
        /// What the records say about the message, apart from the default: the status a consent
        /// policy rule reads.
        recorded: RecordedConsent,
    },
    NoSuchPerson,
    /// The topic is not in the workspace, or belongs to another channel.
    NoSuchTopic,
    /// A dimension or value of the message is not a live one of the workspace.
    NoSuchDimension,
}

/// Whether a send on `channel` for a message with this `topic` and these `dimensions` is
/// permitted for `person`, by the pure read rule over the person's records for the channel.
/// The topic joins the message's coordinates as the built-in `topic` dimension. A message
/// without a topic is judged under an opt-in policy: only a covering subscription allows it.
/// The person is followed through `merged_into`.
pub async fn consent_read(
    pool: &PgPool,
    workspace: WorkspaceId,
    person: Uuid,
    channel: Channel,
    topic: Option<Uuid>,
    dimensions: &Dimensions,
) -> Result<ReadOutcome, StoreError> {
    let mut connection = pool.acquire().await.map_err(StoreError::query)?;
    let mut message = dimensions.clone();
    let policy = match topic {
        Some(id) => match find_topic(&mut *connection, workspace, id).await? {
            Some(found) if found.channel == channel => {
                message.insert(TOPIC_DIMENSION.to_owned(), found.key);
                found.policy
            }
            _ => return Ok(ReadOutcome::NoSuchTopic),
        },
        None => TopicPolicy::OptIn,
    };
    if unknown_dimension_entry(&mut connection, workspace, dimensions)
        .await?
        .is_some()
    {
        return Ok(ReadOutcome::NoSuchDimension);
    }
    let Some(person) = current_person(&mut *connection, workspace, person).await? else {
        return Ok(ReadOutcome::NoSuchPerson);
    };
    let rows: Vec<RecordRow> = sqlx::query_as(
        "SELECT t.key AS topic_key, c.dimensions, c.status, c.changed_at, c.last_change_id \
             FROM consent c \
             LEFT JOIN topics t ON t.workspace_id = c.workspace_id AND t.id = c.topic_id \
             WHERE c.workspace_id = $1 AND c.person_id = $2 AND c.channel = $3",
    )
    .bind(workspace.as_uuid())
    .bind(person)
    .bind(channel.as_str())
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let mut records = Vec::with_capacity(rows.len());
    for row in rows {
        let mut scope = dimensions_of(&row.dimensions);
        if let Some(key) = row.topic_key {
            scope.insert(TOPIC_DIMENSION.to_owned(), key);
        }
        let status: ConsentStatus = row
            .status
            .parse()
            .map_err(|_| StoreError::query(sqlx::Error::Decode("unknown consent status".into())))?;
        records.push(Record {
            scope,
            state: State {
                status,
                changed_at: row.changed_at,
            },
            change_id: row.last_change_id.unwrap_or_else(Uuid::nil),
        });
    }
    Ok(ReadOutcome::Read {
        verdict: read(&message, &records, policy),
        policy,
        recorded: recorded_consent(&message, &records),
    })
}

/// A page of a person's history, newest first by `(occurred_at, id)`, at most `limit`, after
/// the `before` cursor. Only changes scoped to at least the `within` dimension values are
/// listed, so an empty map lists them all. The flag is true when more existed. The person is
/// followed through `merged_into`; the history of a merged person moved to the survivor.
/// `None` when the workspace has no such person.
pub async fn consent_history(
    pool: &PgPool,
    workspace: WorkspaceId,
    person: Uuid,
    limit: u32,
    before: Option<(OffsetDateTime, Uuid)>,
    within: &Dimensions,
) -> Result<Option<(Vec<ConsentHistoryEntry>, bool)>, StoreError> {
    let Some(person) = current_person(pool, workspace, person).await? else {
        return Ok(None);
    };
    let mut rows: Vec<ConsentHistoryEntry> = sqlx::query_as(
        "SELECT c.id, c.channel, c.topic_id, t.key AS topic_key, c.dimensions, c.from_status, \
                c.to_status, c.source, c.evidence, c.occurred_at, c.actor_user_id \
         FROM consent_changes c \
         LEFT JOIN topics t ON t.workspace_id = c.workspace_id AND t.id = c.topic_id \
         WHERE c.workspace_id = $1 AND c.person_id = $2 \
           AND ($3::timestamptz IS NULL OR (c.occurred_at, c.id) < ($3, $4)) \
           AND c.dimensions @> $6 \
         ORDER BY c.occurred_at DESC, c.id DESC LIMIT $5",
    )
    .bind(workspace.as_uuid())
    .bind(person)
    .bind(before.map(|(at, _)| at))
    .bind(before.map(|(_, id)| id))
    .bind(i64::from(limit) + 1)
    .bind(dimensions_json(within))
    .fetch_all(pool)
    .await
    .map_err(StoreError::query)?;
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    Ok(Some((rows, more)))
}

#[cfg(test)]
mod tests {
    use crate::decision_testing::PERSONAL_DATA;

    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::consent::TopicPolicy;
    use zirv_flow_core::{Cause, Slug, WorkspaceName};

    use super::*;
    use crate::people::{ProfileOutcome, ProfileRecord, apply_profile_batch};
    use crate::{NewTopic, begin, commit, create_topic, create_workspace, list_trace};

    const T1: OffsetDateTime = datetime!(2026-01-01 00:00 UTC);
    const T2: OffsetDateTime = datetime!(2026-01-02 00:00 UTC);

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    async fn person(pool: &PgPool, ws: WorkspaceId, email: &str) -> Uuid {
        let record = ProfileRecord {
            source: "crm".into(),
            stream: "contacts".into(),
            record_key: email.into(),
            identifiers: vec![("email".into(), email.into())],
            traits: serde_json::Map::new(),
            observed_at: datetime!(2026-01-01 00:00 UTC),
            trace: TraceContext::root(Cause::api("t")),
        };
        match apply_profile_batch(pool, ws, &[record])
            .await
            .unwrap()
            .remove(0)
        {
            ProfileOutcome::Applied { person_id, .. }
            | ProfileOutcome::Unchanged { person_id, .. } => person_id,
            ProfileOutcome::Rejected { reason } => panic!("rejected: {reason}"),
        }
    }

    async fn topic(pool: &PgPool, ws: WorkspaceId, key: &str) -> Uuid {
        create_topic(
            pool,
            ws,
            &NewTopic {
                key: key.into(),
                name: key.into(),
                description: String::new(),
                channel: Channel::Email,
                policy: TopicPolicy::OptOut,
            },
        )
        .await
        .unwrap()
        .id
    }

    fn change(
        person: Uuid,
        topic: Option<Uuid>,
        status: ConsentStatus,
        at: OffsetDateTime,
    ) -> ConsentChange {
        ConsentChange {
            dimensions: Default::default(),
            person,
            channel: Channel::Email,
            topic,
            status,
            source: ChangeSource::Api,
            evidence: json!({"form": "footer"}),
            occurred_at: at,
            actor: None,
        }
    }

    async fn set(
        pool: &PgPool,
        ws: WorkspaceId,
        change: &ConsentChange,
        trace: &TraceContext,
    ) -> ConsentOutcome {
        let mut tx = begin(pool).await.unwrap();
        let outcome = set_consent(&mut tx, ws, change, trace).await.unwrap();
        commit(tx).await.unwrap();
        outcome
    }

    async fn count(pool: &PgPool, table: &'static str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_change_is_stored_with_history_and_a_decision_that_holds_ids_only(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "one").await;
            let ada = crate::decision_testing::seed_personal_data(&pool, ws).await;
            let news = topic(&pool, ws, "news").await;
            let trace = TraceContext::root(Cause::api("test"));

            let outcome = set(
                &pool,
                ws,
                &change(ada, Some(news), ConsentStatus::Subscribed, T1),
                &trace,
            )
            .await;

            let ConsentOutcome::Changed {
                person_id,
                change_id,
            } = outcome
            else {
                panic!("{outcome:?}");
            };
            assert_eq!(person_id, ada);
            let entries = consent_for(&pool, ws, ada).await.unwrap().unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].topic_key.as_deref(), Some("news"));
            assert_eq!(entries[0].status, "subscribed");
            assert!(entries[0].explicit);
            assert_eq!(entries[0].changed_at, T1);
            let (history, more) = consent_history(&pool, ws, ada, 10, None, &Default::default())
                .await
                .unwrap()
                .unwrap();
            assert!(!more);
            assert_eq!(history.len(), 1);
            assert_eq!(history[0].id, change_id);
            assert_eq!(history[0].from_status, None);
            assert_eq!(history[0].to_status, "subscribed");
            assert_eq!(history[0].source, "api");
            assert_eq!(history[0].evidence, json!({"form": "footer"}));

            let (decisions, _) = list_trace(&pool, ws, trace.trace_id, 20).await.unwrap();
            let decision = decisions
                .iter()
                .find(|d| d.action == "consent.change")
                .unwrap();
            assert_eq!(decision.reason.as_deref(), Some("api"));
            assert_eq!(
                decision.subject_ids,
                json!({"person": ada.to_string(), "consent_change": change_id.to_string()})
            );
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn writing_the_status_already_held_leaves_no_history_and_no_decision(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "one").await;
            let ada = person(&pool, ws, PERSONAL_DATA.email).await;
            let trace = TraceContext::root(Cause::api("test"));
            set(
                &pool,
                ws,
                &change(ada, None, ConsentStatus::Subscribed, T1),
                &trace,
            )
            .await;
            let decisions_before = count(&pool, "decision_records").await;

            let outcome = set(
                &pool,
                ws,
                &change(ada, None, ConsentStatus::Subscribed, T2),
                &trace,
            )
            .await;

            assert_eq!(outcome, ConsentOutcome::Unchanged);
            assert_eq!(count(&pool, "consent_changes").await, 1);
            assert_eq!(count(&pool, "decision_records").await, decisions_before);
            let changed_at: OffsetDateTime = sqlx::query_scalar("SELECT changed_at FROM consent")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(changed_at, T1);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_second_change_records_where_it_came_from_and_the_history_pages_newest_first(
        pool: PgPool,
    ) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "one").await;
            let ada = person(&pool, ws, PERSONAL_DATA.email).await;
            let trace = TraceContext::root(Cause::api("test"));
            set(
                &pool,
                ws,
                &change(ada, None, ConsentStatus::Subscribed, T1),
                &trace,
            )
            .await;
            let mut second = change(ada, None, ConsentStatus::Unsubscribed, T2);
            second.source = ChangeSource::UnsubscribeLink;
            set(&pool, ws, &second, &trace).await;

            let (page, more) = consent_history(&pool, ws, ada, 1, None, &Default::default())
                .await
                .unwrap()
                .unwrap();

            assert!(more);
            assert_eq!(page[0].to_status, "unsubscribed");
            assert_eq!(page[0].from_status.as_deref(), Some("subscribed"));
            assert_eq!(page[0].source, "unsubscribe_link");
            let (rest, more) = consent_history(
                &pool,
                ws,
                ada,
                1,
                Some((page[0].occurred_at, page[0].id)),
                &Default::default(),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(!more);
            assert_eq!(rest[0].to_status, "subscribed");
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_channel_wide_state_and_a_topic_state_are_separate_rows(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "one").await;
            let ada = person(&pool, ws, PERSONAL_DATA.email).await;
            let news = topic(&pool, ws, "news").await;
            let trace = TraceContext::root(Cause::api("test"));

            set(
                &pool,
                ws,
                &change(ada, None, ConsentStatus::Unsubscribed, T1),
                &trace,
            )
            .await;
            set(
                &pool,
                ws,
                &change(ada, Some(news), ConsentStatus::Subscribed, T1),
                &trace,
            )
            .await;

            let entries = consent_for(&pool, ws, ada).await.unwrap().unwrap();
            let shown: Vec<(Option<&str>, &str)> = entries
                .iter()
                .map(|entry| (entry.topic_key.as_deref(), entry.status.as_str()))
                .collect();
            assert_eq!(
                shown,
                [(None, "unsubscribed"), (Some("news"), "subscribed")]
            );
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_topic_of_another_workspace_or_an_unknown_person_changes_nothing(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let one = workspace(&pool, "one").await;
            let two = workspace(&pool, "two").await;
            let ada = person(&pool, one, PERSONAL_DATA.email).await;
            let foreign_topic = topic(&pool, two, "news").await;
            let trace = TraceContext::root(Cause::api("test"));

            let wrong_topic = set(
                &pool,
                one,
                &change(ada, Some(foreign_topic), ConsentStatus::Subscribed, T1),
                &trace,
            )
            .await;
            let wrong_person = set(
                &pool,
                two,
                &change(ada, None, ConsentStatus::Subscribed, T1),
                &trace,
            )
            .await;
            let nobody = set(
                &pool,
                one,
                &change(Uuid::now_v7(), None, ConsentStatus::Subscribed, T1),
                &trace,
            )
            .await;

            assert_eq!(wrong_topic, ConsentOutcome::NoSuchTopic);
            assert_eq!(wrong_person, ConsentOutcome::NoSuchPerson);
            assert_eq!(nobody, ConsentOutcome::NoSuchPerson);
            assert_eq!(count(&pool, "consent").await, 0);
            assert!(consent_for(&pool, two, ada).await.unwrap().is_none());
            assert!(
                consent_history(&pool, two, ada, 5, None, &Default::default())
                    .await
                    .unwrap()
                    .is_none()
            );
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_write_for_a_merged_person_lands_on_the_survivor(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
        let ws = workspace(&pool, "one").await;
        let survivor = person(&pool, ws, PERSONAL_DATA.email).await;
        let merged = person(&pool, ws, "ada2@example.com").await;
        sqlx::query("UPDATE people SET merged_into = $1 WHERE id = $2")
            .bind(survivor)
            .bind(merged)
            .execute(&pool)
            .await
            .unwrap();
        let trace = TraceContext::root(Cause::api("test"));

        let outcome = set(
            &pool,
            ws,
            &change(merged, None, ConsentStatus::Unsubscribed, T1),
            &trace,
        )
        .await;

        assert!(
            matches!(outcome, ConsentOutcome::Changed { person_id, .. } if person_id == survivor)
        );
        let through_merged = consent_for(&pool, ws, merged).await.unwrap().unwrap();
        assert_eq!(through_merged.len(), 1);
        let owner: Uuid = sqlx::query_scalar("SELECT person_id FROM consent")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(owner, survivor);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_history_cannot_be_updated_but_can_be_deleted_for_erasure(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "one").await;
            let ada = person(&pool, ws, PERSONAL_DATA.email).await;
            let trace = TraceContext::root(Cause::api("test"));
            set(
                &pool,
                ws,
                &change(ada, None, ConsentStatus::Subscribed, T1),
                &trace,
            )
            .await;

            let rewrite = sqlx::query("UPDATE consent_changes SET to_status = 'unsubscribed'")
                .execute(&pool)
                .await;
            let evidence = sqlx::query("UPDATE consent_changes SET evidence = '{}'")
                .execute(&pool)
                .await;
            let repoint = sqlx::query("UPDATE consent_changes SET person_id = person_id")
                .execute(&pool)
                .await;
            let stranger = person(&pool, ws, "stranger@example.com").await;
            let to_stranger = sqlx::query("UPDATE consent_changes SET person_id = $1")
                .bind(stranger)
                .execute(&pool)
                .await;
            sqlx::query("DELETE FROM consent_changes")
                .execute(&pool)
                .await
                .unwrap();

            assert!(rewrite.unwrap_err().to_string().contains("append-only"));
            assert!(evidence.is_err());
            assert!(repoint.is_ok());
            assert!(to_stranger.unwrap_err().to_string().contains("survivor"));
            assert_eq!(count(&pool, "consent_changes").await, 0);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn history_moves_only_to_the_survivor_of_a_merge(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "one").await;
            let loser = person(&pool, ws, PERSONAL_DATA.email).await;
            let survivor = person(&pool, ws, "ada2@example.com").await;
            let stranger = person(&pool, ws, "bob@example.com").await;
            let trace = TraceContext::root(Cause::api("test"));
            set(
                &pool,
                ws,
                &change(loser, None, ConsentStatus::Subscribed, T1),
                &trace,
            )
            .await;
            sqlx::query("UPDATE people SET merged_into = $1 WHERE id = $2")
                .bind(survivor)
                .bind(loser)
                .execute(&pool)
                .await
                .unwrap();

            let to_stranger = sqlx::query("UPDATE consent_changes SET person_id = $1")
                .bind(stranger)
                .execute(&pool)
                .await;
            let to_survivor = sqlx::query("UPDATE consent_changes SET person_id = $1")
                .bind(survivor)
                .execute(&pool)
                .await;

            assert!(to_stranger.is_err());
            assert_eq!(to_survivor.unwrap().rows_affected(), 1);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn evidence_that_is_not_an_object_is_refused_by_the_database(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "one").await;
            let ada = person(&pool, ws, PERSONAL_DATA.email).await;
            let trace = TraceContext::root(Cause::api("test"));
            let mut tx = begin(&pool).await.unwrap();
            let mut array = change(ada, None, ConsentStatus::Subscribed, T1);
            array.evidence = json!(["x"]);

            let not_object = set_consent(&mut tx, ws, &array, &trace).await;

            assert!(not_object.is_err());
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_consent_change_marks_the_person_in_the_consent_class_and_a_repeat_marks_nobody(
        pool: PgPool,
    ) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "one").await;
            let ada = person(&pool, ws, PERSONAL_DATA.email).await;
            let trace = TraceContext::root(Cause::api("consent"));
            let change = change(ada, None, ConsentStatus::Subscribed, T1);
            sqlx::query("DELETE FROM person_changes")
                .execute(&pool)
                .await
                .unwrap();

            set(&pool, ws, &change, &trace).await;

            let marks: Vec<(Uuid, i16, Option<Uuid>, Option<String>)> =
                sqlx::query_as("SELECT person_id, classes, trace_id, cause_id FROM person_changes")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            assert_eq!(
                marks,
                [(
                    ada,
                    8,
                    Some(trace.trace_id.as_uuid()),
                    Some("consent".into())
                )]
            );
            sqlx::query("DELETE FROM person_changes")
                .execute(&pool)
                .await
                .unwrap();

            set(&pool, ws, &change, &trace).await;

            assert_eq!(count(&pool, "person_changes").await, 0);
        })
        .await;
    }

    fn scoped(pairs: &[(&str, &str)]) -> Dimensions {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn scoped_change(
        person: Uuid,
        topic: Option<Uuid>,
        pairs: &[(&str, &str)],
        status: ConsentStatus,
        at: OffsetDateTime,
    ) -> ConsentChange {
        ConsentChange {
            dimensions: scoped(pairs),
            ..change(person, topic, status, at)
        }
    }

    /// A `vertical` dimension with the values `retail` and `b2b`.
    async fn verticals(pool: &PgPool, ws: WorkspaceId) {
        use crate::{NewDimension, create_dimension, create_dimension_value};
        create_dimension(
            pool,
            ws,
            &NewDimension {
                key: "vertical".into(),
                name: "Vertical".into(),
            },
        )
        .await
        .unwrap();
        for key in ["retail", "b2b"] {
            create_dimension_value(pool, ws, "vertical", key, key)
                .await
                .unwrap();
        }
    }

    async fn allowed_for(
        pool: &PgPool,
        ws: WorkspaceId,
        person: Uuid,
        topic: Option<Uuid>,
        pairs: &[(&str, &str)],
    ) -> bool {
        match consent_read(pool, ws, person, Channel::Email, topic, &scoped(pairs))
            .await
            .unwrap()
        {
            ReadOutcome::Read { verdict, .. } => verdict.allowed,
            other => panic!("{other:?}"),
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_vertical_scoped_unsubscribe_blocks_only_that_vertical_and_the_rest_stays_separate(
        pool: PgPool,
    ) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "one").await;
            verticals(&pool, ws).await;
            let ada = person(&pool, ws, PERSONAL_DATA.email).await;
            let news = topic(&pool, ws, "news").await;
            let trace = TraceContext::root(Cause::api("test"));
            let retail = [("vertical", "retail")];

            set(
                &pool,
                ws,
                &change(ada, Some(news), ConsentStatus::Subscribed, T1),
                &trace,
            )
            .await;
            let scoped_out = set(
                &pool,
                ws,
                &scoped_change(ada, Some(news), &retail, ConsentStatus::Unsubscribed, T2),
                &trace,
            )
            .await;

            assert!(matches!(scoped_out, ConsentOutcome::Changed { .. }));
            assert_eq!(count(&pool, "consent").await, 2);
            assert!(allowed_for(&pool, ws, ada, Some(news), &[]).await);
            assert!(allowed_for(&pool, ws, ada, Some(news), &[("vertical", "b2b")]).await);
            assert!(!allowed_for(&pool, ws, ada, Some(news), &retail).await);
            let again = set(
                &pool,
                ws,
                &scoped_change(ada, Some(news), &retail, ConsentStatus::Unsubscribed, T2),
                &trace,
            )
            .await;
            assert_eq!(again, ConsentOutcome::Unchanged);
            let entries = consent_for(&pool, ws, ada).await.unwrap().unwrap();
            let shown: Vec<(String, &str)> = entries
                .iter()
                .map(|entry| (entry.dimensions.to_string(), entry.status.as_str()))
                .collect();
            assert_eq!(
                shown,
                [
                    ("{}".to_owned(), "subscribed"),
                    (r#"{"vertical":"retail"}"#.to_owned(), "unsubscribed")
                ]
            );

            set(
                &pool,
                ws,
                &change(
                    ada,
                    Some(news),
                    ConsentStatus::Unsubscribed,
                    T2 + time::Duration::DAY,
                ),
                &trace,
            )
            .await;
            assert!(!allowed_for(&pool, ws, ada, Some(news), &[("vertical", "b2b")]).await);
            let (history, _) = consent_history(&pool, ws, ada, 10, None, &scoped(&retail))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(history.len(), 1);
            assert_eq!(history[0].dimensions, json!({"vertical": "retail"}));
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_export_pages_every_state_in_a_stable_order(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "one").await;
            verticals(&pool, ws).await;
            let ada = person(&pool, ws, PERSONAL_DATA.email).await;
            let bob = person(&pool, ws, "bob@example.com").await;
            let news = topic(&pool, ws, "news").await;
            let trace = TraceContext::root(Cause::api("test"));
            let retail = [("vertical", "retail")];
            for (who, topic, pairs) in [
                (ada, None, &[][..]),
                (ada, Some(news), &[][..]),
                (ada, Some(news), &retail[..]),
                (bob, None, &retail[..]),
            ] {
                set(
                    &pool,
                    ws,
                    &scoped_change(who, topic, pairs, ConsentStatus::Subscribed, T1),
                    &trace,
                )
                .await;
            }

            let mut seen = Vec::new();
            let mut cursor = None;
            loop {
                let page = consent_export_page(&pool, ws, cursor.as_ref(), 3)
                    .await
                    .unwrap();
                let Some(last) = page.last() else { break };
                cursor = Some(last.cursor());
                seen.extend(page);
            }

            let shown: Vec<(Uuid, Option<&str>, &str)> = seen
                .iter()
                .map(|row| {
                    (
                        row.person_id,
                        row.topic_key.as_deref(),
                        row.dimensions_key.as_str(),
                    )
                })
                .collect();
            let mut ordered = vec![
                (ada, None, ""),
                (ada, Some("news"), ""),
                (ada, Some("news"), "vertical=retail"),
                (bob, None, "vertical=retail"),
            ];
            ordered.sort_by_key(|(who, topic, dims)| (*who, topic.unwrap_or(""), *dims));
            assert_eq!(shown, ordered);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_unknown_dimension_or_value_changes_nothing(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "one").await;
            verticals(&pool, ws).await;
            let ada = person(&pool, ws, PERSONAL_DATA.email).await;
            let trace = TraceContext::root(Cause::api("test"));

            let mut refused = Vec::new();
            for pairs in [
                &[("vertical", "unheard-of")][..],
                &[("brand", "x")][..],
                &[("topic", "news")][..],
            ] {
                refused.push(
                    set(
                        &pool,
                        ws,
                        &scoped_change(ada, None, pairs, ConsentStatus::Unsubscribed, T1),
                        &trace,
                    )
                    .await,
                );
            }
            let read = consent_read(
                &pool,
                ws,
                ada,
                Channel::Email,
                None,
                &scoped(&[("brand", "x")]),
            )
            .await
            .unwrap();

            assert_eq!(
                refused,
                [
                    ConsentOutcome::NoSuchDimensionValue,
                    ConsentOutcome::NoSuchDimension,
                    ConsentOutcome::NoSuchDimension
                ]
            );
            assert_eq!(read, ReadOutcome::NoSuchDimension);
            assert_eq!(count(&pool, "consent").await, 0);
            assert_eq!(count(&pool, "consent_changes").await, 0);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn topic_consent_written_without_dimensions_reads_as_before(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "one").await;
            let ada = person(&pool, ws, PERSONAL_DATA.email).await;
            let news = topic(&pool, ws, "news").await;
            sqlx::query(
            "INSERT INTO consent (workspace_id, person_id, channel, topic_id, status, explicit, \
                                  changed_at) \
             VALUES ($1, $2, 'email', $3, 'unsubscribed', true, $4)",
        )
        .bind(ws.as_uuid())
        .bind(ada)
        .bind(news)
        .bind(T1)
        .execute(&pool)
        .await
        .unwrap();

            let entries = consent_for(&pool, ws, ada).await.unwrap().unwrap();

            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].dimensions, json!({}));
            assert_eq!(entries[0].topic_key.as_deref(), Some("news"));
            assert_eq!(entries[0].status, "unsubscribed");
            assert_eq!(entries[0].changed_at, T1);
            assert!(!allowed_for(&pool, ws, ada, Some(news), &[]).await);
            let key: String = sqlx::query_scalar("SELECT dimensions_key FROM consent")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(key, "");
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn equal_maps_in_any_key_order_are_one_row_and_the_empty_scope_stays_unique(
        pool: PgPool,
    ) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "one").await;
            let ada = person(&pool, ws, PERSONAL_DATA.email).await;
            let insert = async |dimensions: &str| {
                sqlx::query(
                    "INSERT INTO consent (workspace_id, person_id, channel, topic_id, dimensions, \
                                      status, explicit, changed_at) \
                 VALUES ($1, $2, 'email', NULL, $3::jsonb, 'subscribed', true, $4)",
                )
                .bind(ws.as_uuid())
                .bind(ada)
                .bind(dimensions)
                .bind(T1)
                .execute(&pool)
                .await
            };

            assert!(insert("{}").await.is_ok());
            assert!(insert("{}").await.is_err());
            assert!(insert(r#"{"a": "x", "bb": "y"}"#).await.is_ok());
            assert!(insert(r#"{"bb":"y","a":"x"}"#).await.is_err());
            assert!(insert(r#"{"a": "x"}"#).await.is_ok());
            let keys: Vec<String> =
                sqlx::query_scalar("SELECT dimensions_key FROM consent ORDER BY dimensions_key")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            assert_eq!(keys, ["", "a=x", "a=x;bb=y"]);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_older_change_is_kept_in_the_history_but_never_replaces_a_newer_state(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
        let ws = workspace(&pool, "one").await;
        verticals(&pool, ws).await;
        let ada = person(&pool, ws, PERSONAL_DATA.email).await;
        let news = topic(&pool, ws, "news").await;
        let trace = TraceContext::root(Cause::api("test"));
        let retail = [("vertical", "retail")];

        for pairs in [&[][..], &retail[..]] {
            set(
                &pool,
                ws,
                &scoped_change(ada, None, pairs, ConsentStatus::Subscribed, T2),
                &trace,
            )
            .await;
            let late = set(
                &pool,
                ws,
                &scoped_change(ada, None, pairs, ConsentStatus::Unsubscribed, T1),
                &trace,
            )
            .await;
            assert_eq!(late, ConsentOutcome::Superseded, "{pairs:?}");
            let (status, at): (String, OffsetDateTime) = sqlx::query_as(
                "SELECT status, changed_at FROM consent WHERE dimensions_key = consent_dimensions_canonical($1)",
            )
            .bind(dimensions_json(&scoped(pairs)))
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!((status.as_str(), at), ("subscribed", T2), "{pairs:?}");
            assert!(
                allowed_for(&pool, ws, ada, Some(news), pairs).await,
                "{pairs:?}"
            );
        }
        assert_eq!(count(&pool, "consent_changes").await, 4);
        let kept: Vec<String> =
            sqlx::query_scalar("SELECT to_status FROM consent_changes WHERE occurred_at = $1")
                .bind(T1)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(kept, ["unsubscribed", "unsubscribed"]);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn reaffirming_the_held_status_advances_it_when_a_covered_record_is_newer(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "one").await;
            verticals(&pool, ws).await;
            let ada = person(&pool, ws, PERSONAL_DATA.email).await;
            let trace = TraceContext::root(Cause::api("test"));
            let retail = [("vertical", "retail")];
            let t3 = T2 + time::Duration::DAY;
            set(
                &pool,
                ws,
                &change(ada, None, ConsentStatus::Unsubscribed, T1),
                &trace,
            )
            .await;
            set(
                &pool,
                ws,
                &scoped_change(ada, None, &retail, ConsentStatus::Subscribed, T2),
                &trace,
            )
            .await;
            assert!(allowed_for(&pool, ws, ada, None, &retail).await);

            let again = set(
                &pool,
                ws,
                &change(ada, None, ConsentStatus::Unsubscribed, t3),
                &trace,
            )
            .await;

            assert!(matches!(again, ConsentOutcome::Changed { .. }), "{again:?}");
            assert!(!allowed_for(&pool, ws, ada, None, &retail).await);
            assert_eq!(count(&pool, "consent_changes").await, 3);
            let plain = set(
                &pool,
                ws,
                &change(
                    ada,
                    None,
                    ConsentStatus::Unsubscribed,
                    t3 + time::Duration::DAY,
                ),
                &trace,
            )
            .await;
            assert_eq!(plain, ConsentOutcome::Unchanged);
            assert_eq!(count(&pool, "consent_changes").await, 3);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_database_refuses_a_dimension_map_that_is_not_strings_of_the_key_form(
        pool: PgPool,
    ) {
        crate::decision_testing::checked(pool, |pool| async move {
        let ws = workspace(&pool, "one").await;
        let ada = person(&pool, ws, PERSONAL_DATA.email).await;
        let state = async |dimensions: &str| {
            sqlx::query(
                "INSERT INTO consent (workspace_id, person_id, channel, dimensions, status,                                       explicit, changed_at)                  VALUES ($1, $2, 'email', $3::jsonb, 'subscribed', true, $4)",
            )
            .bind(ws.as_uuid())
            .bind(ada)
            .bind(dimensions)
            .bind(T1)
            .execute(&pool)
            .await
        };
        let history = async |dimensions: &str| {
            sqlx::query(
                "INSERT INTO consent_changes (id, workspace_id, person_id, channel, dimensions,                                               to_status, source, occurred_at, trace_id)                  VALUES ($1, $2, $3, 'email', $4::jsonb, 'subscribed', 'api', $5, $1)",
            )
            .bind(Uuid::now_v7())
            .bind(ws.as_uuid())
            .bind(ada)
            .bind(dimensions)
            .bind(T1)
            .execute(&pool)
            .await
        };

        for bad in [
            r#"{"a": null}"#,
            r#"{"a":"b;c=d"}"#,
            r#"{"a": 1}"#,
            r#"{"A":"x"}"#,
            r#"{"a":""}"#,
            r#"{"a":"B"}"#,
        ] {
            assert!(state(bad).await.is_err(), "consent {bad}");
            assert!(history(bad).await.is_err(), "history {bad}");
        }
        assert!(state(r#"{"vertical":"retail-eu_1"}"#).await.is_ok());
        assert!(history(r#"{"vertical":"retail-eu_1"}"#).await.is_ok());
        assert!(state("{}").await.is_ok());
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_reaffirm_matters_only_when_an_overlapping_record_of_another_status_lies_between(
        pool: PgPool,
    ) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "one").await;
            verticals(&pool, ws).await;
            let ada = person(&pool, ws, PERSONAL_DATA.email).await;
            let news = topic(&pool, ws, "news").await;
            let trace = TraceContext::root(Cause::api("test"));
            let retail = [("vertical", "retail")];
            let day = time::Duration::DAY;
            let (t3, t4) = (T2 + day, T2 + day + day);

            // A broader record in between: retail subscribed, all-values unsubscribed, retail again.
            set(
                &pool,
                ws,
                &scoped_change(ada, None, &retail, ConsentStatus::Subscribed, T1),
                &trace,
            )
            .await;
            set(
                &pool,
                ws,
                &change(ada, None, ConsentStatus::Unsubscribed, T2),
                &trace,
            )
            .await;
            assert!(!allowed_for(&pool, ws, ada, None, &retail).await);
            let again = set(
                &pool,
                ws,
                &scoped_change(ada, None, &retail, ConsentStatus::Subscribed, t3),
                &trace,
            )
            .await;
            assert!(matches!(again, ConsentOutcome::Changed { .. }), "{again:?}");
            assert!(allowed_for(&pool, ws, ada, None, &retail).await);

            // The same with a topic: news subscribed, channel-wide unsubscribed, news again.
            set(
                &pool,
                ws,
                &change(ada, Some(news), ConsentStatus::Subscribed, T1),
                &trace,
            )
            .await;
            set(
                &pool,
                ws,
                &change(ada, None, ConsentStatus::Unsubscribed, t3),
                &trace,
            )
            .await;
            assert!(!allowed_for(&pool, ws, ada, Some(news), &[]).await);
            let news_again = set(
                &pool,
                ws,
                &change(ada, Some(news), ConsentStatus::Subscribed, t4),
                &trace,
            )
            .await;
            assert!(
                matches!(news_again, ConsentOutcome::Changed { .. }),
                "{news_again:?}"
            );
            assert!(allowed_for(&pool, ws, ada, Some(news), &[]).await);

            // Nothing in between: newer than everything, or older than the held row, writes nothing.
            let before = count(&pool, "consent_changes").await;
            let newest = set(
                &pool,
                ws,
                &change(ada, Some(news), ConsentStatus::Subscribed, t4 + day),
                &trace,
            )
            .await;
            let old = set(
                &pool,
                ws,
                &change(ada, Some(news), ConsentStatus::Subscribed, T1),
                &trace,
            )
            .await;
            assert_eq!(
                (newest, old),
                (ConsentOutcome::Unchanged, ConsentOutcome::Unchanged)
            );
            assert_eq!(count(&pool, "consent_changes").await, before);

            // A record that does not overlap (another vertical) never counts.
            let bob = person(&pool, ws, "bob@example.com").await;
            set(
                &pool,
                ws,
                &scoped_change(bob, None, &retail, ConsentStatus::Subscribed, T1),
                &trace,
            )
            .await;
            set(
                &pool,
                ws,
                &scoped_change(
                    bob,
                    None,
                    &[("vertical", "b2b")],
                    ConsentStatus::Unsubscribed,
                    T2,
                ),
                &trace,
            )
            .await;
            let before = count(&pool, "consent_changes").await;
            let apart = set(
                &pool,
                ws,
                &scoped_change(bob, None, &retail, ConsentStatus::Subscribed, t3),
                &trace,
            )
            .await;
            assert_eq!(apart, ConsentOutcome::Unchanged);
            assert_eq!(count(&pool, "consent_changes").await, before);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_record_with_a_later_id_at_the_change_time_does_not_make_a_reaffirm_matter(
        pool: PgPool,
    ) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "one").await;
            verticals(&pool, ws).await;
            let ada = person(&pool, ws, PERSONAL_DATA.email).await;
            let trace = TraceContext::root(Cause::api("test"));
            let retail = [("vertical", "retail")];
            set(
                &pool,
                ws,
                &scoped_change(ada, None, &retail, ConsentStatus::Subscribed, T1),
                &trace,
            )
            .await;
            // An unsubscribed all-values record at T2 whose change id is later than any id the
            // incoming change can get: a v7 id with the largest timestamp.
            let later = Uuid::from_u128(
                (0xFFFF_FFFF_FFFF_u128 << 80) | (0x7_u128 << 76) | (0x2_u128 << 62),
            );
            sqlx::query(
                "INSERT INTO consent_changes (id, workspace_id, person_id, channel, dimensions, \
                to_status, source, occurred_at, trace_id) \
             VALUES ($1, $2, $3, 'email', '{}'::jsonb, 'unsubscribed', 'api', $4, $1)",
            )
            .bind(later)
            .bind(ws.as_uuid())
            .bind(ada)
            .bind(T2)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
            "INSERT INTO consent (workspace_id, person_id, channel, dimensions, status, explicit, \
                changed_at, last_change_id) \
             VALUES ($1, $2, 'email', '{}'::jsonb, 'unsubscribed', true, $3, $4)",
        )
        .bind(ws.as_uuid())
        .bind(ada)
        .bind(T2)
        .bind(later)
        .execute(&pool)
        .await
        .unwrap();
            let before = count(&pool, "consent_changes").await;

            let outcome = set(
                &pool,
                ws,
                &scoped_change(ada, None, &retail, ConsentStatus::Subscribed, T2),
                &trace,
            )
            .await;

            assert_eq!(outcome, ConsentOutcome::Unchanged);
            assert_eq!(count(&pool, "consent_changes").await, before);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn consent_superseded(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "fixture").await;
            let person = person(&pool, ws, PERSONAL_DATA.email).await;
            set(
                &pool,
                ws,
                &change(person, None, ConsentStatus::Subscribed, T2),
                &TraceContext::root(Cause::api("newer")),
            )
            .await;
            let trace = TraceContext::root(Cause::api("late"));
            let outcome = set(
                &pool,
                ws,
                &change(person, None, ConsentStatus::Unsubscribed, T1),
                &trace,
            )
            .await;
            assert_eq!(outcome, ConsentOutcome::Superseded);
            let change: Uuid = sqlx::query_scalar(
                "SELECT id FROM consent_changes WHERE person_id = $1 AND occurred_at = $2",
            )
            .bind(person)
            .bind(T1)
            .fetch_one(&pool)
            .await
            .unwrap();
            crate::decision_testing::assert_fixture(
                &pool,
                ws,
                &trace,
                ("consent.change", "superseded", Some("api")),
                json!({"person": person, "consent_change": change}),
            )
            .await;
        })
        .await;
    }
}
