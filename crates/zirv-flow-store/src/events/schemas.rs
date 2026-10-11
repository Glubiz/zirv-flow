//! The schema registry and the workspace event policy (architecture section 12.1).

use std::collections::HashMap;

use serde_json::Value;
use sqlx::prelude::FromRow;
use sqlx::types::Json;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::events::{EventMode, RedactionRules, SchemaProperty, UnplannedPolicy};

use crate::StoreError;

macro_rules! schema_columns {
    () => {
        "workspace_id, name, mode, draft, current_version, retention_days, description, \
         inferred, created_at, updated_at"
    };
}

/// A schema entry: its mode, its editable draft and the number of its latest version.
#[derive(Clone, Debug, PartialEq)]
pub struct EventSchema {
    pub workspace_id: WorkspaceId,
    pub name: String,
    pub mode: EventMode,
    pub draft: Option<Vec<SchemaProperty>>,
    pub current_version: Option<i32>,
    pub retention_days: Option<i32>,
    pub description: String,
    /// The draft was inferred from observed events.
    pub inferred: bool,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// One published, immutable version.
#[derive(Clone, Debug, PartialEq)]
pub struct SchemaVersion {
    pub version: i32,
    pub properties: Vec<SchemaProperty>,
    pub published_at: OffsetDateTime,
    pub published_by: Option<Uuid>,
}

/// A schema to create. The name and the properties are already validated.
#[derive(Clone, Debug)]
pub struct NewEventSchema {
    pub name: String,
    pub mode: EventMode,
    pub description: String,
    pub properties: Vec<SchemaProperty>,
}

/// The mutable fields of a schema entry; `None` leaves a field as it is. For the retention,
/// `Some(None)` clears the override.
#[derive(Clone, Debug, Default)]
pub struct EventSchemaUpdate {
    pub mode: Option<EventMode>,
    pub description: Option<String>,
    pub retention_days: Option<Option<i32>>,
}

/// The result of publishing a draft.
#[derive(Clone, Debug, PartialEq)]
pub enum PublishOutcome {
    Published(EventSchema),
    NoDraft,
    NotFound,
}

/// The workspace policy for unplanned names, time and redaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventPolicy {
    pub unplanned: UnplannedPolicy,
    pub future_skew_seconds: i32,
    pub lateness_seconds: i32,
    pub redaction: RedactionRules,
}

impl Default for EventPolicy {
    fn default() -> Self {
        Self {
            unplanned: UnplannedPolicy::Draft,
            future_skew_seconds: 300,
            lateness_seconds: 86_400,
            redaction: RedactionRules::default(),
        }
    }
}

#[derive(FromRow)]
struct SchemaRow {
    workspace_id: Uuid,
    name: String,
    mode: String,
    draft: Option<Json<Vec<SchemaProperty>>>,
    current_version: Option<i32>,
    retention_days: Option<i32>,
    description: String,
    inferred: bool,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

fn corrupt(what: &'static str) -> StoreError {
    StoreError::query(sqlx::Error::Decode(what.into()))
}

impl SchemaRow {
    fn into_schema(self) -> Result<EventSchema, StoreError> {
        Ok(EventSchema {
            workspace_id: WorkspaceId::from_uuid(self.workspace_id),
            name: self.name,
            mode: self
                .mode
                .parse()
                .map_err(|_| corrupt("unknown event mode"))?,
            draft: self.draft.map(|draft| draft.0),
            current_version: self.current_version,
            retention_days: self.retention_days,
            description: self.description,
            inferred: self.inferred,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

#[derive(FromRow)]
struct VersionRow {
    version: i32,
    properties: Json<Vec<SchemaProperty>>,
    published_at: OffsetDateTime,
    published_by: Option<Uuid>,
}

/// Every schema entry of the workspace, ordered by name.
pub async fn list_event_schemas<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
) -> Result<Vec<EventSchema>, StoreError> {
    let rows: Vec<SchemaRow> = sqlx::query_as(concat!(
        "SELECT ",
        schema_columns!(),
        " FROM event_schemas WHERE workspace_id = $1 ORDER BY name"
    ))
    .bind(workspace.as_uuid())
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    rows.into_iter().map(SchemaRow::into_schema).collect()
}

pub async fn find_event_schema<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    name: &str,
) -> Result<Option<EventSchema>, StoreError> {
    let row: Option<SchemaRow> = sqlx::query_as(concat!(
        "SELECT ",
        schema_columns!(),
        " FROM event_schemas WHERE workspace_id = $1 AND name = $2"
    ))
    .bind(workspace.as_uuid())
    .bind(name)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    row.map(SchemaRow::into_schema).transpose()
}

/// The published versions of a schema, newest first.
pub async fn list_schema_versions<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    name: &str,
) -> Result<Vec<SchemaVersion>, StoreError> {
    let rows: Vec<VersionRow> = sqlx::query_as(
        "SELECT version, properties, published_at, published_by FROM event_schema_versions \
         WHERE workspace_id = $1 AND name = $2 ORDER BY version DESC",
    )
    .bind(workspace.as_uuid())
    .bind(name)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(rows
        .into_iter()
        .map(|row| SchemaVersion {
            version: row.version,
            properties: row.properties.0,
            published_at: row.published_at,
            published_by: row.published_by,
        })
        .collect())
}

/// Creates a schema entry with `properties` as its draft. A name that exists is a conflict.
pub async fn create_event_schema<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    new: &NewEventSchema,
) -> Result<EventSchema, StoreError> {
    let row: SchemaRow = sqlx::query_as(concat!(
        "INSERT INTO event_schemas (workspace_id, name, mode, description, draft) \
         VALUES ($1, $2, $3, $4, $5) RETURNING ",
        schema_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(&new.name)
    .bind(new.mode.as_str())
    .bind(&new.description)
    .bind(Json(&new.properties))
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)?;
    row.into_schema()
}

/// Replaces the draft. The draft is no longer an inferred one. `None` when there is no
/// such schema.
pub async fn save_schema_draft<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    name: &str,
    properties: &[SchemaProperty],
) -> Result<Option<EventSchema>, StoreError> {
    let row: Option<SchemaRow> = sqlx::query_as(concat!(
        "UPDATE event_schemas SET draft = $3, inferred = false, updated_at = now() \
         WHERE workspace_id = $1 AND name = $2 RETURNING ",
        schema_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(name)
    .bind(Json(properties))
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    row.map(SchemaRow::into_schema).transpose()
}

/// Publishes the draft as the next version and clears the draft. Run it in a transaction
/// together with its audit entry.
pub async fn publish_schema_draft(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    name: &str,
    published_by: Option<Uuid>,
) -> Result<PublishOutcome, StoreError> {
    let locked: Option<SchemaRow> = sqlx::query_as(concat!(
        "SELECT ",
        schema_columns!(),
        " FROM event_schemas WHERE workspace_id = $1 AND name = $2 FOR UPDATE"
    ))
    .bind(workspace.as_uuid())
    .bind(name)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let Some(locked) = locked else {
        return Ok(PublishOutcome::NotFound);
    };
    let Some(Json(draft)) = locked.draft else {
        return Ok(PublishOutcome::NoDraft);
    };
    let version = locked.current_version.unwrap_or(0) + 1;
    sqlx::query(
        "INSERT INTO event_schema_versions \
            (workspace_id, name, version, properties, published_by) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(workspace.as_uuid())
    .bind(name)
    .bind(version)
    .bind(Json(&draft))
    .bind(published_by)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let row: SchemaRow = sqlx::query_as(concat!(
        "UPDATE event_schemas SET draft = NULL, inferred = false, current_version = $3, \
            updated_at = now() \
         WHERE workspace_id = $1 AND name = $2 RETURNING ",
        schema_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(name)
    .bind(version)
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(PublishOutcome::Published(row.into_schema()?))
}

/// Applies the given fields. `None` when there is no such schema. A change to the retention
/// override first takes the lock a partition drop holds, so it waits for the drop to finish.
pub async fn update_event_schema(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    name: &str,
    update: &EventSchemaUpdate,
) -> Result<Option<EventSchema>, StoreError> {
    if update.retention_days.is_some() {
        super::retention::lock_retention_overrides(connection).await?;
    }
    let row: Option<SchemaRow> = sqlx::query_as(concat!(
        "UPDATE event_schemas SET \
            mode = COALESCE($3, mode), \
            description = COALESCE($4, description), \
            retention_days = CASE WHEN $5 THEN $6 ELSE retention_days END, \
            updated_at = now() \
         WHERE workspace_id = $1 AND name = $2 RETURNING ",
        schema_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(name)
    .bind(update.mode.map(EventMode::as_str))
    .bind(update.description.as_deref())
    .bind(update.retention_days.is_some())
    .bind(update.retention_days.flatten())
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?;
    row.map(SchemaRow::into_schema).transpose()
}

/// Drafts a schema from an unplanned event, unless the name already has an entry.
pub(crate) async fn draft_inferred_schema(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    name: &str,
    properties: &[SchemaProperty],
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO event_schemas (workspace_id, name, draft, inferred) \
         VALUES ($1, $2, $3, true) ON CONFLICT (workspace_id, name) DO NOTHING",
    )
    .bind(workspace.as_uuid())
    .bind(name)
    .bind(Json(properties))
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// A schema with the properties of its current version, as validation needs it.
#[derive(Clone, Debug)]
pub(crate) struct PublishedSchema {
    pub mode: EventMode,
    pub version: i32,
    pub properties: Vec<SchemaProperty>,
}

/// The published schema of a name; `None` when the name has no published version.
pub(crate) async fn find_published_schema(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    name: &str,
) -> Result<Option<PublishedSchema>, StoreError> {
    let row: Option<(String, i32, Json<Vec<SchemaProperty>>)> = sqlx::query_as(
        "SELECT s.mode, v.version, v.properties FROM event_schemas s \
         JOIN event_schema_versions v \
           ON v.workspace_id = s.workspace_id AND v.name = s.name AND v.version = s.current_version \
         WHERE s.workspace_id = $1 AND s.name = $2",
    )
    .bind(workspace.as_uuid())
    .bind(name)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?;
    row.map(|(mode, version, properties)| {
        Ok(PublishedSchema {
            mode: mode.parse().map_err(|_| corrupt("unknown event mode"))?,
            version,
            properties: properties.0,
        })
    })
    .transpose()
}

/// The published schemas of `names` in one query. A name without a published version is
/// absent from the map.
pub(crate) async fn find_published_schemas(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    names: &[String],
) -> Result<HashMap<String, PublishedSchema>, StoreError> {
    let rows: Vec<(String, String, i32, Json<Vec<SchemaProperty>>)> = sqlx::query_as(
        "SELECT s.name, s.mode, v.version, v.properties FROM event_schemas s \
         JOIN event_schema_versions v \
           ON v.workspace_id = s.workspace_id AND v.name = s.name AND v.version = s.current_version \
         WHERE s.workspace_id = $1 AND s.name = ANY($2)",
    )
    .bind(workspace.as_uuid())
    .bind(names)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    rows.into_iter()
        .map(|(name, mode, version, properties)| {
            Ok((
                name,
                PublishedSchema {
                    mode: mode.parse().map_err(|_| corrupt("unknown event mode"))?,
                    version,
                    properties: properties.0,
                },
            ))
        })
        .collect()
}

/// The workspace's policy, or the defaults when none was saved.
pub async fn get_event_policy<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
) -> Result<EventPolicy, StoreError> {
    let row: Option<(String, i32, i32, Value)> = sqlx::query_as(
        "SELECT unplanned, future_skew_seconds, lateness_seconds, redaction \
         FROM event_policies WHERE workspace_id = $1",
    )
    .bind(workspace.as_uuid())
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    let Some((unplanned, future_skew_seconds, lateness_seconds, redaction)) = row else {
        return Ok(EventPolicy::default());
    };
    Ok(EventPolicy {
        unplanned: unplanned
            .parse()
            .map_err(|_| corrupt("unknown unplanned policy"))?,
        future_skew_seconds,
        lateness_seconds,
        redaction: serde_json::from_value(redaction)
            .map_err(|_| corrupt("unreadable redaction rules"))?,
    })
}

/// Saves the policy, replacing the previous one.
pub async fn set_event_policy<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    policy: &EventPolicy,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO event_policies \
            (workspace_id, unplanned, future_skew_seconds, lateness_seconds, redaction) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (workspace_id) DO UPDATE SET unplanned = EXCLUDED.unplanned, \
            future_skew_seconds = EXCLUDED.future_skew_seconds, \
            lateness_seconds = EXCLUDED.lateness_seconds, \
            redaction = EXCLUDED.redaction, updated_at = now()",
    )
    .bind(workspace.as_uuid())
    .bind(policy.unplanned.as_str())
    .bind(policy.future_skew_seconds)
    .bind(policy.lateness_seconds)
    .bind(Json(&policy.redaction))
    .execute(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use zirv_flow_core::events::PropertyType;
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;
    use crate::create_workspace;

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    fn property(name: &str, required: bool) -> SchemaProperty {
        SchemaProperty {
            name: name.to_owned(),
            property_type: PropertyType::String,
            required,
            enum_values: None,
            description: String::new(),
            pii: false,
        }
    }

    fn new_schema(name: &str, properties: Vec<SchemaProperty>) -> NewEventSchema {
        NewEventSchema {
            name: name.to_owned(),
            mode: EventMode::Block,
            description: "d".to_owned(),
            properties,
        }
    }

    #[test]
    fn the_default_policy_drafts_unplanned_names_and_truncates_ips() {
        let policy = EventPolicy::default();

        assert_eq!(policy.unplanned, UnplannedPolicy::Draft);
        assert_eq!(
            (policy.future_skew_seconds, policy.lateness_seconds),
            (300, 86_400)
        );
        assert_eq!(policy.redaction.truncate_ip, ["ip", "context.ip"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_schema_is_created_drafted_published_and_versioned(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let created =
            create_event_schema(&pool, ws, &new_schema("signup", vec![property("a", true)]))
                .await
                .unwrap();
        assert_eq!(created.mode, EventMode::Block);
        assert_eq!(created.draft.as_ref().unwrap().len(), 1);
        assert_eq!(created.current_version, None);
        let duplicate = create_event_schema(&pool, ws, &new_schema("signup", Vec::new()))
            .await
            .unwrap_err();
        assert!(duplicate.is_conflict());

        let mut tx = crate::begin(&pool).await.unwrap();
        let first = publish_schema_draft(&mut tx, ws, "signup", None)
            .await
            .unwrap();
        let again = publish_schema_draft(&mut tx, ws, "signup", None)
            .await
            .unwrap();
        let missing = publish_schema_draft(&mut tx, ws, "nope", None)
            .await
            .unwrap();
        crate::commit(tx).await.unwrap();

        let PublishOutcome::Published(published) = first else {
            panic!("not published");
        };
        assert_eq!(
            (published.current_version, published.draft.is_none()),
            (Some(1), true)
        );
        assert_eq!(again, PublishOutcome::NoDraft);
        assert_eq!(missing, PublishOutcome::NotFound);

        save_schema_draft(
            &pool,
            ws,
            "signup",
            &[property("a", true), property("b", false)],
        )
        .await
        .unwrap()
        .unwrap();
        let mut tx = crate::begin(&pool).await.unwrap();
        publish_schema_draft(&mut tx, ws, "signup", None)
            .await
            .unwrap();
        crate::commit(tx).await.unwrap();
        let versions = list_schema_versions(&pool, ws, "signup").await.unwrap();
        let numbers: Vec<i32> = versions.iter().map(|version| version.version).collect();
        assert_eq!(numbers, [2, 1]);
        assert_eq!(versions[0].properties.len(), 2);
        assert_eq!(versions[1].properties.len(), 1);
        let published = find_published_schema(&mut pool.acquire().await.unwrap(), ws, "signup")
            .await
            .unwrap()
            .unwrap();
        assert_eq!((published.version, published.properties.len()), (2, 2));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_version_cannot_be_updated(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        create_event_schema(&pool, ws, &new_schema("signup", Vec::new()))
            .await
            .unwrap();
        let mut tx = crate::begin(&pool).await.unwrap();
        publish_schema_draft(&mut tx, ws, "signup", None)
            .await
            .unwrap();
        crate::commit(tx).await.unwrap();

        let error = sqlx::query("UPDATE event_schema_versions SET properties = '[]'::jsonb")
            .execute(&pool)
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("event schema versions are immutable")
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_entry_updates_its_mode_description_and_retention(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        create_event_schema(&pool, ws, &new_schema("signup", Vec::new()))
            .await
            .unwrap();

        let set = update_event_schema(
            &mut pool.acquire().await.unwrap(),
            ws,
            "signup",
            &EventSchemaUpdate {
                mode: Some(EventMode::Allow),
                retention_days: Some(Some(90)),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
        let untouched = update_event_schema(
            &mut pool.acquire().await.unwrap(),
            ws,
            "signup",
            &EventSchemaUpdate {
                description: Some("new".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
        let cleared = update_event_schema(
            &mut pool.acquire().await.unwrap(),
            ws,
            "signup",
            &EventSchemaUpdate {
                retention_days: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
        let missing = update_event_schema(
            &mut pool.acquire().await.unwrap(),
            ws,
            "nope",
            &EventSchemaUpdate::default(),
        )
        .await
        .unwrap();

        assert_eq!((set.mode, set.retention_days), (EventMode::Allow, Some(90)));
        assert_eq!(
            (untouched.retention_days, untouched.description.as_str()),
            (Some(90), "new")
        );
        assert_eq!(cleared.retention_days, None);
        assert!(missing.is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_policy_defaults_until_saved_and_is_replaced_whole(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        assert_eq!(
            get_event_policy(&pool, ws).await.unwrap(),
            EventPolicy::default()
        );
        let policy = EventPolicy {
            unplanned: UnplannedPolicy::Reject,
            future_skew_seconds: 10,
            lateness_seconds: 20,
            redaction: RedactionRules {
                drop: vec!["card".into()],
                hash: vec!["email".into()],
                truncate_ip: Vec::new(),
            },
        };

        set_event_policy(&pool, ws, &policy).await.unwrap();
        set_event_policy(&pool, ws, &policy).await.unwrap();

        assert_eq!(get_event_policy(&pool, ws).await.unwrap(), policy);
        let other = workspace(&pool, "two").await;
        assert_eq!(
            get_event_policy(&pool, other).await.unwrap(),
            EventPolicy::default()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_inferred_draft_never_overwrites_an_existing_entry(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        create_event_schema(&pool, ws, &new_schema("signup", vec![property("a", true)]))
            .await
            .unwrap();
        let mut connection = pool.acquire().await.unwrap();

        draft_inferred_schema(&mut connection, ws, "signup", &[property("z", false)])
            .await
            .unwrap();
        draft_inferred_schema(&mut connection, ws, "fresh", &[property("z", false)])
            .await
            .unwrap();

        let existing = find_event_schema(&pool, ws, "signup")
            .await
            .unwrap()
            .unwrap();
        let fresh = find_event_schema(&pool, ws, "fresh")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(existing.draft.unwrap()[0].name, "a");
        assert!(!existing.inferred && fresh.inferred);
        assert_eq!(fresh.mode, EventMode::Warn);
        assert!(
            find_published_schema(&mut connection, ws, "fresh")
                .await
                .unwrap()
                .is_none()
        );
    }
}
