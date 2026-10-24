//! The workspace facts a segment definition is checked against: the input save and preview
//! share, so they cannot disagree about what exists.

use hashbrown::{HashMap, HashSet};
use sqlx::PgConnection;
use sqlx::types::Json;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::consent::Channel;
use zirv_flow_core::events::{PropertyType, SchemaProperty};
use zirv_flow_core::people::{SlotColumn, TraitType};

use crate::{
    StoreError, list_lists, list_segments, list_topics, list_trait_definitions, ready_trait_slots,
};

/// What a workspace defines now: traits that are not archived, the properties of each event
/// schema's current version, list ids, the ids of segments that are not archived and the
/// consent topics of each channel. The API turns it into the compiler's catalog.
#[derive(Clone, Debug, Default)]
pub struct CatalogSnapshot {
    pub traits: HashMap<String, TraitType>,
    pub events: HashMap<String, HashMap<String, PropertyType>>,
    pub lists: HashSet<Uuid>,
    pub segments: HashSet<Uuid>,
    /// Each channel's topics: a key with its id.
    pub topics: HashMap<Channel, HashMap<String, Uuid>>,
    /// The live consent dimensions: a key with its live value keys.
    pub dimensions: HashMap<String, HashSet<String>>,
    /// The trait slots in state `ready`, which the compiler may read. They come from the same
    /// snapshot as the traits.
    pub slots: Vec<(String, SlotColumn)>,
}

/// Reads the snapshot on `connection`. Run it inside the transaction that uses the result:
/// preview reads it in its repeatable-read transaction, save in the transaction that holds the
/// segments lock.
pub async fn segment_catalog(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<CatalogSnapshot, StoreError> {
    // A statement cancelled at the preview's timeout is a timeout, not a failed query.
    read_catalog(connection, workspace)
        .await
        .map_err(StoreError::into_bounded)
}

/// The catalog and the database clock, read on one connection of `pool` that is released when the
/// call returns. For a check that writes nothing and so needs no transaction of its own: the
/// assistant's `draft_segment` tool validates a draft with it.
pub async fn read_segment_catalog(
    pool: &sqlx::PgPool,
    workspace: WorkspaceId,
) -> Result<(CatalogSnapshot, time::OffsetDateTime), StoreError> {
    let mut connection = pool.acquire().await.map_err(StoreError::query)?;
    let snapshot = segment_catalog(&mut connection, workspace).await?;
    let now = crate::database_now(&mut *connection).await?;
    Ok((snapshot, now))
}

async fn read_catalog(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<CatalogSnapshot, StoreError> {
    let mut snapshot = CatalogSnapshot::default();
    for definition in list_trait_definitions(&mut *connection, workspace).await? {
        if !definition.archived {
            snapshot
                .traits
                .insert(definition.key, definition.trait_type);
        }
    }

    let schemas: Vec<(String, Json<Vec<SchemaProperty>>)> = sqlx::query_as(
        "SELECT s.name, v.properties FROM event_schemas s \
         JOIN event_schema_versions v ON v.workspace_id = s.workspace_id \
                                     AND v.name = s.name AND v.version = s.current_version \
         WHERE s.workspace_id = $1",
    )
    .bind(workspace.as_uuid())
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    for (name, Json(properties)) in schemas {
        snapshot.events.insert(
            name,
            properties
                .into_iter()
                .map(|property| (property.name, property.property_type))
                .collect(),
        );
    }

    let lists: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM lists WHERE workspace_id = $1")
        .bind(workspace.as_uuid())
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    snapshot.lists.extend(lists);

    let segments: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM segments WHERE workspace_id = $1 AND archived_at IS NULL",
    )
    .bind(workspace.as_uuid())
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    snapshot.segments.extend(segments);

    for topic in list_topics(&mut *connection, workspace, false).await? {
        snapshot
            .topics
            .entry(topic.channel)
            .or_default()
            .insert(topic.key, topic.id);
    }
    let dimensions: Vec<(String, String)> = sqlx::query_as(
        "SELECT d.key, v.key FROM consent_dimensions d          JOIN consent_dimension_values v             ON v.workspace_id = d.workspace_id AND v.dimension_id = d.id          WHERE d.workspace_id = $1 AND d.archived_at IS NULL AND v.archived_at IS NULL",
    )
    .bind(workspace.as_uuid())
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    for (dimension, value) in dimensions {
        snapshot
            .dimensions
            .entry(dimension)
            .or_default()
            .insert(value);
    }
    snapshot.slots = ready_trait_slots(&mut *connection, workspace).await?;
    Ok(snapshot)
}

/// A trait of the registry as the assistant is told about it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TraitDetail {
    pub key: String,
    pub trait_type: TraitType,
    pub description: String,
    /// Declared personal data. A trait that is not declared non-personal counts as personal.
    pub pii: bool,
}

/// A property of an event's current schema version.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PropertyDetail {
    pub name: String,
    pub property_type: PropertyType,
    pub pii: bool,
}

/// An event name with the properties of its current schema version.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventDetail {
    pub name: String,
    pub properties: Vec<PropertyDetail>,
}

/// A consent topic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TopicDetail {
    pub channel: Channel,
    pub key: String,
    pub name: String,
}

/// The names and descriptions behind [`CatalogSnapshot`], in a stable order, for telling the
/// assistant what a definition may refer to. It holds definitions of the workspace's model and no
/// value of any person, event or list member.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CatalogDetails {
    pub traits: Vec<TraitDetail>,
    pub events: Vec<EventDetail>,
    pub lists: Vec<(Uuid, String)>,
    pub segments: Vec<(Uuid, String)>,
    pub topics: Vec<TopicDetail>,
}

/// Reads the details on `connection`, with the same filters as [`segment_catalog`]: traits that
/// are not archived, the current version of each event schema, and segments that are not
/// archived.
pub async fn segment_catalog_details(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<CatalogDetails, StoreError> {
    let mut details = CatalogDetails::default();
    for definition in list_trait_definitions(&mut *connection, workspace).await? {
        if !definition.archived {
            details.traits.push(TraitDetail {
                key: definition.key,
                trait_type: definition.trait_type,
                description: definition.description,
                pii: definition.pii,
            });
        }
    }
    details.traits.sort_by(|a, b| a.key.cmp(&b.key));

    let schemas: Vec<(String, Json<Vec<SchemaProperty>>)> = sqlx::query_as(
        "SELECT s.name, v.properties FROM event_schemas s \n         JOIN event_schema_versions v ON v.workspace_id = s.workspace_id \n                                     AND v.name = s.name AND v.version = s.current_version \n         WHERE s.workspace_id = $1 ORDER BY s.name",
    )
    .bind(workspace.as_uuid())
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    for (name, Json(properties)) in schemas {
        details.events.push(EventDetail {
            name,
            properties: properties
                .into_iter()
                .map(|property| PropertyDetail {
                    name: property.name,
                    property_type: property.property_type,
                    pii: property.pii,
                })
                .collect(),
        });
    }

    for list in list_lists(&mut *connection, workspace).await? {
        details.lists.push((list.id, list.name));
    }
    for segment in list_segments(&mut *connection, workspace, false).await? {
        details.segments.push((segment.id, segment.name));
    }
    for topic in list_topics(&mut *connection, workspace, false).await? {
        details.topics.push(TopicDetail {
            channel: topic.channel,
            key: topic.key,
            name: topic.name,
        });
    }
    Ok(details)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use zirv_flow_core::consent::TopicPolicy;
    use zirv_flow_core::events::EventMode;
    use zirv_flow_core::people::Precedence;
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;
    use crate::{
        NewEventSchema, NewTopic, NewTraitDefinition, TraitDefinitionUpdate, create_event_schema,
        create_list, create_topic, create_trait_definition, create_workspace, publish_schema_draft,
        update_trait_definition,
    };

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    fn new_trait(key: &str) -> NewTraitDefinition {
        NewTraitDefinition {
            key: key.into(),
            trait_type: TraitType::Number,
            precedence: Precedence::Latest,
            source_order: Vec::new(),
            pii: false,
            channel_address: false,
            inferred: false,
            description: String::new(),
        }
    }

    #[test]
    fn a_default_snapshot_is_empty() {
        let snapshot = CatalogSnapshot::default();
        assert!(snapshot.traits.is_empty() && snapshot.events.is_empty());
        assert!(snapshot.lists.is_empty() && snapshot.segments.is_empty());
        assert!(snapshot.topics.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_snapshot_holds_the_workspace_and_nothing_of_another(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        for key in ["age", "gone"] {
            create_trait_definition(&pool, one, &new_trait(key))
                .await
                .unwrap();
        }
        create_trait_definition(&pool, two, &new_trait("other"))
            .await
            .unwrap();
        update_trait_definition(
            &pool,
            one,
            "gone",
            &TraitDefinitionUpdate {
                archived: Some(true),
                ..TraitDefinitionUpdate::default()
            },
        )
        .await
        .unwrap();
        let list = create_list(&pool, one, "VIPs", "", None).await.unwrap();
        create_list(&pool, two, "Theirs", "", None).await.unwrap();
        let schema = |properties: &str| NewEventSchema {
            name: "order".into(),
            mode: EventMode::Warn,
            description: String::new(),
            properties: serde_json::from_str(properties).unwrap(),
        };
        create_event_schema(
            &pool,
            one,
            &schema(r#"[{"name": "total", "type": "number"}]"#),
        )
        .await
        .unwrap();
        let mut connection = pool.acquire().await.unwrap();
        publish_schema_draft(&mut connection, one, "order", None)
            .await
            .unwrap();
        let topic = create_topic(
            &pool,
            one,
            &NewTopic {
                key: "news".into(),
                name: "News".into(),
                description: String::new(),
                channel: Channel::Email,
                policy: TopicPolicy::OptIn,
            },
        )
        .await
        .unwrap();

        let snapshot = segment_catalog(&mut connection, one).await.unwrap();

        assert_eq!(snapshot.traits.len(), 1);
        assert_eq!(snapshot.traits.get("age"), Some(&TraitType::Number));
        assert_eq!(snapshot.lists.len(), 1);
        assert!(snapshot.lists.contains(&list.id));
        assert_eq!(
            snapshot.events["order"].get("total"),
            Some(&PropertyType::Number)
        );
        assert_eq!(snapshot.topics[&Channel::Email]["news"], topic.id);
        assert!(snapshot.segments.is_empty());
        let (read, now) = read_segment_catalog(&pool, one).await.unwrap();
        assert_eq!(read.traits, snapshot.traits);
        assert!(now.year() >= 2026);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_details_name_what_the_snapshot_holds_and_nothing_of_another_workspace(
        pool: PgPool,
    ) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let mut personal = new_trait("city");
        personal.pii = true;
        personal.description = "Where they live".into();
        create_trait_definition(&pool, one, &personal)
            .await
            .unwrap();
        create_trait_definition(&pool, one, &new_trait("age"))
            .await
            .unwrap();
        create_trait_definition(&pool, two, &new_trait("other"))
            .await
            .unwrap();
        let list = create_list(&pool, one, "VIPs", "", None).await.unwrap();
        create_list(&pool, two, "Theirs", "", None).await.unwrap();
        create_event_schema(
            &pool,
            one,
            &NewEventSchema {
                name: "order".into(),
                mode: EventMode::Warn,
                description: String::new(),
                properties: serde_json::from_str(
                    r#"[{"name": "total", "type": "number"},
                        {"name": "coupon", "type": "string", "pii": true}]"#,
                )
                .unwrap(),
            },
        )
        .await
        .unwrap();
        let mut connection = pool.acquire().await.unwrap();
        publish_schema_draft(&mut connection, one, "order", None)
            .await
            .unwrap();

        let details = segment_catalog_details(&mut connection, one).await.unwrap();

        let keys: Vec<(&str, bool)> = details
            .traits
            .iter()
            .map(|entry| (entry.key.as_str(), entry.pii))
            .collect();
        assert_eq!(keys, [("age", false), ("city", true)]);
        assert_eq!(details.traits[1].description, "Where they live");
        assert_eq!(details.lists, [(list.id, "VIPs".to_owned())]);
        assert_eq!(details.events.len(), 1);
        assert_eq!(details.events[0].name, "order");
        let coupon = details.events[0]
            .properties
            .iter()
            .find(|property| property.name == "coupon")
            .unwrap();
        assert!(coupon.pii);
        assert!(details.segments.is_empty() && details.topics.is_empty());
    }
}
