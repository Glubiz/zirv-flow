//! The merge fields a template may use: the facts a publish validates the document against.

use std::collections::BTreeMap;

use sqlx::PgConnection;
use sqlx::types::Json;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::events::{PropertyType, SchemaProperty};
use zirv_flow_core::people::TraitType;

use crate::{StoreError, list_trait_definitions};

/// What the workspace defines now, as ordered maps so the result never depends on read order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TemplateFieldCatalog {
    /// Registry traits that are not archived, by key.
    pub traits: BTreeMap<String, TraitType>,
    /// The properties of every event schema's current version, by name. A template does not know
    /// which event starts its flow, so the properties of all events are offered; when two events
    /// give one name different types, the event that sorts first by name decides.
    pub event_properties: BTreeMap<String, PropertyType>,
}

/// Reads the field catalog on `connection`. Run it inside the transaction that uses the result,
/// so a publish validates against what it publishes with.
pub async fn read_template_field_catalog(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<TemplateFieldCatalog, StoreError> {
    let mut catalog = TemplateFieldCatalog::default();
    for definition in list_trait_definitions(&mut *connection, workspace).await? {
        if !definition.archived {
            catalog.traits.insert(definition.key, definition.trait_type);
        }
    }
    let schemas: Vec<(String, Json<Vec<SchemaProperty>>)> = sqlx::query_as(
        "SELECT s.name, v.properties FROM event_schemas s \
         JOIN event_schema_versions v ON v.workspace_id = s.workspace_id \
                                     AND v.name = s.name AND v.version = s.current_version \
         WHERE s.workspace_id = $1 ORDER BY s.name",
    )
    .bind(workspace.as_uuid())
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    for (_, Json(properties)) in schemas {
        for property in properties {
            catalog
                .event_properties
                .entry(property.name)
                .or_insert(property.property_type);
        }
    }
    Ok(catalog)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use zirv_flow_core::events::EventMode;
    use zirv_flow_core::people::Precedence;

    use super::*;
    use crate::templates::tests_support::workspace;
    use crate::{
        NewEventSchema, NewTraitDefinition, create_event_schema, create_trait_definition,
        publish_schema_draft,
    };

    async fn add_trait(pool: &PgPool, ws: WorkspaceId, key: &str, trait_type: TraitType) {
        create_trait_definition(
            pool,
            ws,
            &NewTraitDefinition {
                key: key.to_owned(),
                trait_type,
                precedence: Precedence::Latest,
                source_order: Vec::new(),
                pii: false,
                channel_address: false,
                inferred: false,
                description: String::new(),
            },
        )
        .await
        .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_catalog_holds_the_workspaces_traits_by_type(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;
        add_trait(&pool, ws, "plan", TraitType::String).await;
        add_trait(&pool, ws, "age", TraitType::Number).await;
        add_trait(&pool, other, "secret", TraitType::String).await;
        let mut connection = pool.acquire().await.unwrap();

        let catalog = read_template_field_catalog(&mut connection, ws)
            .await
            .unwrap();

        assert_eq!(
            catalog.traits.into_iter().collect::<Vec<_>>(),
            [
                ("age".to_owned(), TraitType::Number),
                ("plan".to_owned(), TraitType::String)
            ]
        );
        assert!(catalog.event_properties.is_empty());
    }

    async fn publish_event(
        pool: &PgPool,
        ws: WorkspaceId,
        name: &str,
        properties: &[(&str, PropertyType)],
    ) {
        let properties: Vec<SchemaProperty> = properties
            .iter()
            .map(|(name, property_type)| SchemaProperty {
                name: (*name).to_owned(),
                property_type: *property_type,
                required: false,
                enum_values: None,
                description: String::new(),
                pii: false,
            })
            .collect();
        let new = NewEventSchema {
            name: name.to_owned(),
            mode: EventMode::Warn,
            description: String::new(),
            properties,
        };
        create_event_schema(pool, ws, &new).await.unwrap();
        let mut connection = pool.acquire().await.unwrap();
        publish_schema_draft(&mut connection, ws, name, None)
            .await
            .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn event_properties_are_the_union_of_the_current_schemas(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        publish_event(
            &pool,
            ws,
            "order.placed",
            &[
                ("total", PropertyType::Number),
                ("sku", PropertyType::String),
            ],
        )
        .await;
        publish_event(
            &pool,
            ws,
            "page.viewed",
            &[
                ("total", PropertyType::String),
                ("url", PropertyType::String),
            ],
        )
        .await;
        let mut connection = pool.acquire().await.unwrap();

        let catalog = read_template_field_catalog(&mut connection, ws)
            .await
            .unwrap();

        let names: Vec<&str> = catalog
            .event_properties
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(names, ["sku", "total", "url"]);
        // `order.placed` sorts first, so its type for `total` wins.
        assert_eq!(catalog.event_properties["total"], PropertyType::Number);
    }
}
