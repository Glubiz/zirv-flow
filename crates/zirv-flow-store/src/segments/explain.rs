//! What the explain view reads of one person (architecture 8.3): the person's traits, the events
//! the definition can use, list ids, consent rows, identifier kinds and open memberships, all in
//! the caller's snapshot transaction. The store never sees the evaluator: it returns its own row
//! types and the API turns them into the evaluator's snapshot.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

use serde_json::{Map, Value};
use sqlx::PgConnection;
use sqlx::types::Json;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::consent::{Channel, ConsentStatus};
use zirv_flow_core::events::SchemaProperty;

use crate::StoreError;
use crate::people::find_person;

/// How many events one explain loads in total, newest first. A name with events past it is
/// reported as capped, and the evaluator calls a condition on it unknown (ruling 0c6404db).
pub const EXPLAIN_EVENT_CAP: usize = 10_000;

/// One event name to load, and the earliest `occurred_at` a condition on it can use; `None` has
/// no lower bound.
#[derive(Clone, Debug, PartialEq)]
pub struct ExplainEventScope {
    pub name: String,
    pub since: Option<OffsetDateTime>,
}

/// One loaded event.
#[derive(Clone, PartialEq)]
pub struct ExplainEvent {
    pub name: String,
    pub occurred_at: OffsetDateTime,
    pub properties: Map<String, Value>,
}

/// One consent row: the channel or one of its topics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExplainConsent {
    pub channel: Channel,
    pub topic: Option<Uuid>,
    pub status: ConsentStatus,
}

/// One person as the explain view reads it.
#[derive(Clone, PartialEq)]
pub struct ExplainPerson {
    /// The live person: the survivor when the id asked for was merged away.
    pub id: Uuid,
    /// The id that was asked for, when it was merged into `id`.
    pub merged_from: Option<Uuid>,
    pub traits: Map<String, Value>,
    pub events: Vec<ExplainEvent>,
    /// The scoped names that have events past [`EXPLAIN_EVENT_CAP`].
    pub capped_events: BTreeSet<String>,
    /// Set when the cap cut rows: the `occurred_at` of the oldest row kept. Every event of every
    /// scoped name strictly after it is loaded; at exactly that instant rows may be missing.
    pub events_complete_after: Option<OffsetDateTime>,
    pub lists: BTreeSet<Uuid>,
    pub consent: Vec<ExplainConsent>,
    /// The kinds only, never the values.
    pub identifier_kinds: BTreeSet<String>,
    /// The segments the person is an open member of, with when each membership started.
    pub open_segments: BTreeMap<Uuid, OffsetDateTime>,
}

// Trait values and event properties are personal data.
impl fmt::Debug for ExplainPerson {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExplainPerson")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for ExplainEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExplainEvent")
            .field("name", &self.name)
            .field("occurred_at", &self.occurred_at)
            .finish_non_exhaustive()
    }
}

/// The trait keys and event properties the workspace has declared not personal. Everything else,
/// including a key the registry does not know, counts as personal data.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClearFields {
    traits: BTreeSet<String>,
    event_properties: BTreeSet<(String, String)>,
}

impl ClearFields {
    /// The fields declared not personal: trait keys, and event name with property name.
    pub fn new<'a>(
        traits: impl IntoIterator<Item = &'a str>,
        event_properties: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Self {
        Self {
            traits: traits.into_iter().map(str::to_owned).collect(),
            event_properties: event_properties
                .into_iter()
                .map(|(event, property)| (event.to_owned(), property.to_owned()))
                .collect(),
        }
    }

    pub fn is_trait_pii(&self, key: &str) -> bool {
        !self.traits.contains(key)
    }

    pub fn is_property_pii(&self, event: &str, property: &str) -> bool {
        !self
            .event_properties
            .contains(&(event.to_owned(), property.to_owned()))
    }
}

/// Loads `person` (or its survivor), or `None` when the workspace has no such person. Events are
/// limited to the scoped names and bounds, newest first, [`EXPLAIN_EVENT_CAP`] in total. A
/// statement cancelled at the transaction's timeout is [`StoreError::is_timeout`].
pub async fn load_explain_person(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
    scope: &[ExplainEventScope],
) -> Result<Option<ExplainPerson>, StoreError> {
    let Some(found) = find_person(&mut *connection, workspace, person)
        .await
        .map_err(StoreError::into_bounded)?
    else {
        return Ok(None);
    };
    let traits = match found.traits {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    let (events, capped_events, events_complete_after) =
        load_events(connection, workspace, found.id, scope).await?;

    let lists: Vec<Uuid> = sqlx::query_scalar(
        "SELECT list_id FROM list_members WHERE workspace_id = $1 AND person_id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(found.id)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::bounded)?;

    let rows: Vec<(String, Option<Uuid>, String)> = sqlx::query_as(
        "SELECT channel, topic_id, status FROM consent WHERE workspace_id = $1 AND person_id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(found.id)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::bounded)?;
    let consent = rows
        .into_iter()
        .map(|(channel, topic, status)| {
            Ok(ExplainConsent {
                channel: Channel::from_str(&channel).map_err(decode)?,
                topic,
                status: ConsentStatus::from_str(&status).map_err(decode)?,
            })
        })
        .collect::<Result<Vec<_>, StoreError>>()?;

    // Kinds only: a value is never selected.
    let kinds: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT kind FROM identities WHERE workspace_id = $1 AND person_id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(found.id)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::bounded)?;

    let open: Vec<(Uuid, OffsetDateTime)> = sqlx::query_as(
        "SELECT segment_id, entered_at FROM segment_members \
         WHERE workspace_id = $1 AND person_id = $2 AND exited_at IS NULL",
    )
    .bind(workspace.as_uuid())
    .bind(found.id)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::bounded)?;

    Ok(Some(ExplainPerson {
        id: found.id,
        merged_from: (found.id != person).then_some(person),
        traits,
        events,
        capped_events,
        events_complete_after,
        lists: lists.into_iter().collect(),
        consent,
        identifier_kinds: kinds.into_iter().collect(),
        open_segments: open.into_iter().collect(),
    }))
}

fn decode(error: zirv_flow_core::ValidationError) -> StoreError {
    StoreError::query(sqlx::Error::Decode(Box::new(error)))
}

const SCOPED_EVENTS: &str = "FROM events e \
     JOIN unnest($3::text[], $4::timestamptz[]) AS s(name, since) \
       ON s.name = e.name AND (s.since IS NULL OR e.occurred_at >= s.since) \
     WHERE e.workspace_id = $1 AND e.person_id = $2";

/// One loaded event row, with the id the cap boundary is cut on.
#[derive(sqlx::FromRow)]
struct EventRow {
    id: Uuid,
    name: String,
    occurred_at: OffsetDateTime,
    properties: Json<Map<String, Value>>,
}

async fn load_events(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
    scope: &[ExplainEventScope],
) -> Result<(Vec<ExplainEvent>, BTreeSet<String>, Option<OffsetDateTime>), StoreError> {
    if scope.is_empty() {
        return Ok((Vec::new(), BTreeSet::new(), None));
    }
    let names: Vec<&str> = scope.iter().map(|entry| entry.name.as_str()).collect();
    let since: Vec<Option<OffsetDateTime>> = scope.iter().map(|entry| entry.since).collect();
    let limit = i64::try_from(EXPLAIN_EVENT_CAP + 1).unwrap_or(i64::MAX);
    let sql = format!(
        "SELECT e.id, e.name, e.occurred_at, e.properties {SCOPED_EVENTS} \
         ORDER BY e.occurred_at DESC, e.id DESC LIMIT $5"
    );
    let mut rows: Vec<EventRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(workspace.as_uuid())
        .bind(person)
        .bind(&names)
        .bind(&since)
        .bind(limit)
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::bounded)?;

    let mut capped = BTreeSet::new();
    let mut complete_after = None;
    if rows.len() > EXPLAIN_EVENT_CAP {
        rows.truncate(EXPLAIN_EVENT_CAP);
        if let Some(EventRow {
            id: last_id,
            occurred_at: last_at,
            ..
        }) = rows.last()
        {
            // The names that still have an event older than the oldest one kept.
            let sql = format!(
                "SELECT DISTINCT e.name {SCOPED_EVENTS} AND (e.occurred_at, e.id) < ($5, $6)"
            );
            let cut: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
                .bind(workspace.as_uuid())
                .bind(person)
                .bind(&names)
                .bind(&since)
                .bind(last_at)
                .bind(last_id)
                .fetch_all(&mut *connection)
                .await
                .map_err(StoreError::bounded)?;
            capped.extend(cut);
            complete_after = Some(*last_at);
        }
    }
    let events = rows
        .into_iter()
        .map(
            |EventRow {
                 name,
                 occurred_at,
                 properties,
                 ..
             }| ExplainEvent {
                name,
                occurred_at,
                properties: properties.0,
            },
        )
        .collect();
    Ok((events, capped, complete_after))
}

/// Which trait keys and event properties of the workspace are declared not personal: traits that
/// are not archived and not flagged, and the properties of each event schema's current version
/// that are not flagged.
pub async fn explain_clear_fields(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<ClearFields, StoreError> {
    let traits: Vec<String> = sqlx::query_scalar(
        "SELECT key FROM trait_definitions \
         WHERE workspace_id = $1 AND NOT pii AND NOT archived",
    )
    .bind(workspace.as_uuid())
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::bounded)?;
    let schemas: Vec<(String, Json<Vec<SchemaProperty>>)> = sqlx::query_as(
        "SELECT s.name, v.properties FROM event_schemas s \
         JOIN event_schema_versions v ON v.workspace_id = s.workspace_id \
                                     AND v.name = s.name AND v.version = s.current_version \
         WHERE s.workspace_id = $1",
    )
    .bind(workspace.as_uuid())
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::bounded)?;
    let mut event_properties = BTreeSet::new();
    for (name, Json(properties)) in schemas {
        for property in properties.into_iter().filter(|property| !property.pii) {
            event_properties.insert((name.clone(), property.name));
        }
    }
    Ok(ClearFields {
        traits: traits.into_iter().collect(),
        event_properties,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::Duration;

    use super::*;
    use crate::segments::tests_support::{person, segment, workspace};

    fn scope(name: &str, since: Option<OffsetDateTime>) -> ExplainEventScope {
        ExplainEventScope {
            name: name.to_owned(),
            since,
        }
    }

    /// `count` events, one per second back from now.
    async fn events(pool: &PgPool, ws: WorkspaceId, person: Uuid, name: &str, count: i32) {
        sqlx::query(
            "INSERT INTO events (id, workspace_id, person_id, name, occurred_at, received_at, \
                source, trace_id, validation_state, properties) \
             SELECT gen_random_uuid(), $1, $2, $3, now() - n * interval '1 second', now(), \
                'test', gen_random_uuid(), 'valid', jsonb_build_object('n', n) \
             FROM generate_series(1, $4) n",
        )
        .bind(ws.as_uuid())
        .bind(person)
        .bind(name)
        .bind(count)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn load(
        pool: &PgPool,
        ws: WorkspaceId,
        person: Uuid,
        scope: &[ExplainEventScope],
    ) -> Option<ExplainPerson> {
        let mut tx = crate::begin(pool).await.unwrap();
        load_explain_person(&mut tx, ws, person, scope)
            .await
            .unwrap()
    }

    #[test]
    fn the_debug_form_hides_traits_and_properties() {
        let person = ExplainPerson {
            id: Uuid::nil(),
            merged_from: None,
            traits: json!({"plan": "secret-plan"}).as_object().cloned().unwrap(),
            events: vec![ExplainEvent {
                name: "order".into(),
                occurred_at: OffsetDateTime::UNIX_EPOCH,
                properties: json!({"card": "4111"}).as_object().cloned().unwrap(),
            }],
            capped_events: BTreeSet::new(),
            events_complete_after: None,
            lists: BTreeSet::new(),
            consent: Vec::new(),
            identifier_kinds: BTreeSet::new(),
            open_segments: BTreeMap::new(),
        };

        let text = format!("{person:?} {:?}", person.events[0]);

        assert!(!text.contains("secret-plan") && !text.contains("4111"));
    }

    #[test]
    fn an_unknown_field_counts_as_personal_data() {
        let mut clear = ClearFields::default();
        clear.traits.insert("plan".into());
        clear
            .event_properties
            .insert(("order".into(), "total".into()));

        assert!(!clear.is_trait_pii("plan") && clear.is_trait_pii("email_hash"));
        assert!(!clear.is_property_pii("order", "total"));
        assert!(clear.is_property_pii("order", "card") && clear.is_property_pii("visit", "total"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_person_loads_with_traits_identifier_kinds_and_open_memberships(pool: PgPool) {
        let ws = workspace(&pool, "explain-load").await;
        let ada = person(&pool, ws, "ada@example.com", "pro").await;
        let seg = segment(&pool, ws, "pros", 60).await;
        sqlx::query(
            "INSERT INTO segment_members (workspace_id, segment_id, person_id, version, \
                entered_at, entered_trace_id, entered_cause) \
             VALUES ($1, $2, $3, 1, now(), gen_random_uuid(), 'initial')",
        )
        .bind(ws.as_uuid())
        .bind(seg.id)
        .bind(ada)
        .execute(&pool)
        .await
        .unwrap();

        let loaded = load(&pool, ws, ada, &[]).await.unwrap();

        assert_eq!(loaded.id, ada);
        assert_eq!(loaded.merged_from, None);
        assert_eq!(loaded.traits["plan"], "pro");
        assert_eq!(
            loaded.identifier_kinds,
            BTreeSet::from(["email".to_owned()])
        );
        assert!(loaded.open_segments.contains_key(&seg.id));
        assert!(loaded.events.is_empty() && loaded.capped_events.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_person_of_another_workspace_is_not_found(pool: PgPool) {
        let ours = workspace(&pool, "explain-ours").await;
        let theirs = workspace(&pool, "explain-theirs").await;
        let stranger = person(&pool, theirs, "eve@example.com", "free").await;

        assert!(load(&pool, ours, stranger, &[]).await.is_none());
        assert!(load(&pool, ours, Uuid::now_v7(), &[]).await.is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_merged_person_is_read_as_the_survivor_with_the_asked_id_named(pool: PgPool) {
        let ws = workspace(&pool, "explain-merged").await;
        let survivor = person(&pool, ws, "ada@example.com", "pro").await;
        let loser = person(&pool, ws, "ada.old@example.com", "free").await;
        sqlx::query("UPDATE people SET merged_into = $2 WHERE id = $1")
            .bind(loser)
            .bind(survivor)
            .execute(&pool)
            .await
            .unwrap();

        let loaded = load(&pool, ws, loser, &[]).await.unwrap();

        assert_eq!(loaded.id, survivor);
        assert_eq!(loaded.merged_from, Some(loser));
        assert_eq!(loaded.traits["plan"], "pro");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn only_the_scoped_names_after_their_bound_are_loaded(pool: PgPool) {
        let ws = workspace(&pool, "explain-scope").await;
        let ada = person(&pool, ws, "ada@example.com", "pro").await;
        events(&pool, ws, ada, "order", 10).await;
        events(&pool, ws, ada, "visit", 4).await;
        events(&pool, ws, ada, "other", 3).await;
        let since = OffsetDateTime::now_utc() - Duration::seconds(5);

        let loaded = load(
            &pool,
            ws,
            ada,
            &[scope("order", Some(since)), scope("visit", None)],
        )
        .await
        .unwrap();

        let count = |name: &str| loaded.events.iter().filter(|e| e.name == name).count();
        assert_eq!(count("order"), 4);
        assert_eq!(count("visit"), 4);
        assert_eq!(count("other"), 0);
        assert!(loaded.capped_events.is_empty() && loaded.events_complete_after.is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_name_past_the_cap_is_capped_and_a_small_one_is_not(pool: PgPool) {
        let ws = workspace(&pool, "explain-cap").await;
        let ada = person(&pool, ws, "ada@example.com", "pro").await;
        let big = i32::try_from(EXPLAIN_EVENT_CAP).unwrap() + 1;
        events(&pool, ws, ada, "view", big).await;
        // Newer than every `view`, so the cut falls in `view` alone.
        sqlx::query(
            "INSERT INTO events (id, workspace_id, person_id, name, occurred_at, received_at, \
                source, trace_id, validation_state) \
             VALUES (gen_random_uuid(), $1, $2, 'order', now(), now(), \
                'test', gen_random_uuid(), 'valid')",
        )
        .bind(ws.as_uuid())
        .bind(ada)
        .execute(&pool)
        .await
        .unwrap();

        let loaded = load(&pool, ws, ada, &[scope("view", None), scope("order", None)])
            .await
            .unwrap();

        assert_eq!(loaded.events.len(), EXPLAIN_EVENT_CAP);
        assert_eq!(loaded.capped_events, BTreeSet::from(["view".to_owned()]));
        let oldest_kept = loaded.events.iter().map(|event| event.occurred_at).min();
        assert_eq!(loaded.events_complete_after, oldest_kept);
        assert!(oldest_kept.is_some());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_clear_fields_are_the_unflagged_unarchived_ones(pool: PgPool) {
        let ws = workspace(&pool, "explain-clear").await;
        sqlx::query(
            "INSERT INTO trait_definitions (workspace_id, key, type, pii, archived) VALUES \
             ($1, 'plan', 'string', false, false), ($1, 'phone_note', 'string', true, false), \
             ($1, 'old', 'string', false, true)",
        )
        .bind(ws.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
        let mut conn = pool.acquire().await.unwrap();

        let clear = explain_clear_fields(&mut conn, ws).await.unwrap();

        assert!(!clear.is_trait_pii("plan"));
        assert!(clear.is_trait_pii("phone_note") && clear.is_trait_pii("old"));
    }
}
