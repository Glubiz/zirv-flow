//! The consent dimensions of a workspace (architecture sections 6 and 10.1, OQ-32): named
//! axes such as a vertical or a brand, each with enumerated value keys, that a consent record
//! can be scoped to. The built-in `topic` dimension is resolved from the `topics` rows and is
//! never stored here.

use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::consent::{Dimensions, TOPIC_DIMENSION};

use crate::StoreError;

/// One value of a dimension.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct DimensionValue {
    pub key: String,
    pub name: String,
    pub archived_at: Option<OffsetDateTime>,
}

/// A dimension with its values. The built-in `topic` dimension has no id; its values are the
/// workspace's topics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Dimension {
    pub id: Option<Uuid>,
    pub key: String,
    pub name: String,
    pub builtin: bool,
    pub archived_at: Option<OffsetDateTime>,
    pub values: Vec<DimensionValue>,
}

/// A dimension to declare. The key is already validated.
#[derive(Clone, Debug)]
pub struct NewDimension {
    pub key: String,
    pub name: String,
}

/// The mutable fields of a dimension or of one of its values; `None` leaves a field as it is.
/// The key never changes.
#[derive(Clone, Debug, Default)]
pub struct DimensionUpdate {
    pub name: Option<String>,
    pub archived: Option<bool>,
}

#[derive(FromRow)]
struct DimensionRow {
    id: Uuid,
    key: String,
    name: String,
    archived_at: Option<OffsetDateTime>,
}

#[derive(FromRow)]
struct ValueRow {
    dimension_id: Uuid,
    key: String,
    name: String,
    archived_at: Option<OffsetDateTime>,
}

impl DimensionRow {
    fn into_dimension(self, values: Vec<DimensionValue>) -> Dimension {
        Dimension {
            id: Some(self.id),
            key: self.key,
            name: self.name,
            builtin: false,
            archived_at: self.archived_at,
            values,
        }
    }
}

/// The built-in `topic` dimension of the workspace: one value per topic, by key.
async fn topic_dimension(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    include_archived: bool,
) -> Result<Dimension, StoreError> {
    let values: Vec<DimensionValue> = sqlx::query_as(
        "SELECT key, name, archived_at FROM topics \
         WHERE workspace_id = $1 AND ($2 OR archived_at IS NULL) ORDER BY key",
    )
    .bind(workspace.as_uuid())
    .bind(include_archived)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(Dimension {
        id: None,
        key: TOPIC_DIMENSION.to_owned(),
        name: "Topic".to_owned(),
        builtin: true,
        archived_at: None,
        values,
    })
}

/// The workspace's dimensions: the built-in `topic` first, then the declared ones by key, each
/// with its values by key. Archived dimensions and values are left out unless asked for.
pub async fn list_dimensions(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    include_archived: bool,
) -> Result<Vec<Dimension>, StoreError> {
    let mut all = vec![topic_dimension(&mut *connection, workspace, include_archived).await?];
    let rows: Vec<DimensionRow> = sqlx::query_as(
        "SELECT id, key, name, archived_at FROM consent_dimensions \
         WHERE workspace_id = $1 AND ($2 OR archived_at IS NULL) ORDER BY key",
    )
    .bind(workspace.as_uuid())
    .bind(include_archived)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let values: Vec<ValueRow> = sqlx::query_as(
        "SELECT dimension_id, key, name, archived_at FROM consent_dimension_values \
         WHERE workspace_id = $1 AND ($2 OR archived_at IS NULL) ORDER BY dimension_id, key",
    )
    .bind(workspace.as_uuid())
    .bind(include_archived)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    for row in rows {
        let mine = values
            .iter()
            .filter(|value| value.dimension_id == row.id)
            .map(|value| DimensionValue {
                key: value.key.clone(),
                name: value.name.clone(),
                archived_at: value.archived_at,
            })
            .collect();
        all.push(row.into_dimension(mine));
    }
    Ok(all)
}

/// One dimension by key, `topic` included, with all its values. `None` when the workspace has
/// none.
pub async fn find_dimension(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &str,
) -> Result<Option<Dimension>, StoreError> {
    Ok(list_dimensions(connection, workspace, true)
        .await?
        .into_iter()
        .find(|dimension| dimension.key == key))
}

/// Declares a dimension. A key that exists, archived or not, is a conflict.
pub async fn create_dimension<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    new: &NewDimension,
) -> Result<Dimension, StoreError> {
    let row: DimensionRow = sqlx::query_as(
        "INSERT INTO consent_dimensions (id, workspace_id, key, name) VALUES ($1, $2, $3, $4) \
         RETURNING id, key, name, archived_at",
    )
    .bind(Uuid::now_v7())
    .bind(workspace.as_uuid())
    .bind(&new.key)
    .bind(&new.name)
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(row.into_dimension(Vec::new()))
}

/// Applies the given fields to the declared dimension `key`. `false` when the workspace has no
/// such dimension. Archiving keeps the time of the first archive; un-archiving clears it.
pub async fn update_dimension<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    key: &str,
    update: &DimensionUpdate,
) -> Result<bool, StoreError> {
    let result = sqlx::query(
        "UPDATE consent_dimensions SET \
            name = COALESCE($3, name), \
            archived_at = CASE WHEN $4::boolean IS NULL THEN archived_at \
                               WHEN $4 THEN COALESCE(archived_at, now()) ELSE NULL END, \
            updated_at = now() \
         WHERE workspace_id = $1 AND key = $2",
    )
    .bind(workspace.as_uuid())
    .bind(key)
    .bind(update.name.as_deref())
    .bind(update.archived)
    .execute(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(result.rows_affected() > 0)
}

/// Adds a value to the declared dimension `dimension`. `None` when the workspace has no such
/// dimension; a key that exists, archived or not, is a conflict.
pub async fn create_dimension_value<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    dimension: &str,
    key: &str,
    name: &str,
) -> Result<Option<DimensionValue>, StoreError> {
    sqlx::query_as(
        "INSERT INTO consent_dimension_values (id, workspace_id, dimension_id, key, name) \
         SELECT $1, d.workspace_id, d.id, $4, $5 FROM consent_dimensions d \
         WHERE d.workspace_id = $2 AND d.key = $3 RETURNING key, name, archived_at",
    )
    .bind(Uuid::now_v7())
    .bind(workspace.as_uuid())
    .bind(dimension)
    .bind(key)
    .bind(name)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// Applies the given fields to one value of the declared dimension `dimension`. `None` when
/// there is no such dimension or value.
pub async fn update_dimension_value<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    dimension: &str,
    key: &str,
    update: &DimensionUpdate,
) -> Result<Option<DimensionValue>, StoreError> {
    sqlx::query_as(
        "UPDATE consent_dimension_values v SET \
            name = COALESCE($5, v.name), \
            archived_at = CASE WHEN $4::boolean IS NULL THEN v.archived_at \
                               WHEN $4 THEN COALESCE(v.archived_at, now()) ELSE NULL END, \
            updated_at = now() \
         FROM consent_dimensions d \
         WHERE d.workspace_id = $1 AND d.key = $2 AND v.dimension_id = d.id AND v.key = $3 \
         RETURNING v.key, v.name, v.archived_at",
    )
    .bind(workspace.as_uuid())
    .bind(dimension)
    .bind(key)
    .bind(update.archived)
    .bind(update.name.as_deref())
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// The first entry of `dimensions` that names no live dimension or value of the workspace, as
/// `(dimension key, value key)`, or `None` when every entry is known. The built-in `topic` is
/// not settable through a map: a topic is addressed by its id.
pub async fn unknown_dimension_entry(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    dimensions: &Dimensions,
) -> Result<Option<(String, String)>, StoreError> {
    for (key, value) in dimensions {
        let known: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM consent_dimensions d \
                JOIN consent_dimension_values v ON v.dimension_id = d.id \
                WHERE d.workspace_id = $1 AND d.key = $2 AND v.key = $3 \
                  AND d.archived_at IS NULL AND v.archived_at IS NULL)",
        )
        .bind(workspace.as_uuid())
        .bind(key)
        .bind(value)
        .fetch_one(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        if !known {
            return Ok(Some((key.clone(), value.clone())));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use zirv_flow_core::consent::{Channel, TopicPolicy};
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;
    use crate::consent::{NewTopic, create_topic};
    use crate::create_workspace;

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    fn new(key: &str) -> NewDimension {
        NewDimension {
            key: key.into(),
            name: format!("Name of {key}"),
        }
    }

    fn dims(pairs: &[(&str, &str)]) -> Dimensions {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn archive() -> DimensionUpdate {
        DimensionUpdate {
            name: None,
            archived: Some(true),
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_topic_dimension_is_built_in_and_lists_the_topics_as_its_values(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        create_topic(
            &pool,
            ws,
            &NewTopic {
                key: "news".into(),
                name: "News".into(),
                description: String::new(),
                channel: Channel::Email,
                policy: TopicPolicy::OptOut,
            },
        )
        .await
        .unwrap();
        create_dimension(&pool, ws, &new("vertical")).await.unwrap();
        let mut connection = pool.acquire().await.unwrap();

        let all = list_dimensions(&mut connection, ws, false).await.unwrap();

        let shown: Vec<(&str, bool, Vec<&str>)> = all
            .iter()
            .map(|d| {
                (
                    d.key.as_str(),
                    d.builtin,
                    d.values.iter().map(|v| v.key.as_str()).collect(),
                )
            })
            .collect();
        assert_eq!(
            shown,
            [("topic", true, vec!["news"]), ("vertical", false, vec![])]
        );
        let refused = sqlx::query(
            "INSERT INTO consent_dimensions (id, workspace_id, key, name) \
             VALUES ($1, $2, 'topic', 'x')",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .execute(&pool)
        .await;
        assert!(refused.is_err());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn dimensions_and_values_are_created_updated_archived_and_private_to_a_workspace(
        pool: PgPool,
    ) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        create_dimension(&pool, one, &new("vertical"))
            .await
            .unwrap();
        let duplicate = create_dimension(&pool, one, &new("vertical")).await;
        create_dimension(&pool, two, &new("vertical"))
            .await
            .unwrap();

        let retail = create_dimension_value(&pool, one, "vertical", "retail", "Retail")
            .await
            .unwrap()
            .unwrap();
        let again = create_dimension_value(&pool, one, "vertical", "retail", "Again").await;
        let nowhere = create_dimension_value(&pool, one, "brand", "x", "X")
            .await
            .unwrap();
        let renamed = update_dimension_value(
            &pool,
            one,
            "vertical",
            "retail",
            &DimensionUpdate {
                name: Some("Shops".into()),
                archived: Some(true),
            },
        )
        .await
        .unwrap()
        .unwrap();
        let foreign = update_dimension_value(
            &pool,
            two,
            "vertical",
            "retail",
            &DimensionUpdate::default(),
        )
        .await
        .unwrap();
        let archived = update_dimension(&pool, one, "vertical", &archive())
            .await
            .unwrap();

        assert_eq!(retail.name, "Retail");
        assert!(duplicate.unwrap_err().is_conflict());
        assert!(again.unwrap_err().is_conflict());
        assert!(nowhere.is_none() && foreign.is_none());
        assert_eq!(
            (renamed.name.as_str(), renamed.archived_at.is_some()),
            ("Shops", true)
        );
        assert!(archived);
        let mut connection = pool.acquire().await.unwrap();
        assert_eq!(
            list_dimensions(&mut connection, one, false)
                .await
                .unwrap()
                .len(),
            1
        );
        let hidden = find_dimension(&mut connection, one, "vertical")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(hidden.values.len(), 1);
        assert!(
            find_dimension(&mut connection, two, "brand")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn only_live_declared_dimension_values_are_known(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        create_dimension(&pool, ws, &new("vertical")).await.unwrap();
        for (key, name) in [("retail", "Retail"), ("b2b", "B2B")] {
            create_dimension_value(&pool, ws, "vertical", key, name)
                .await
                .unwrap();
        }
        update_dimension_value(&pool, ws, "vertical", "b2b", &archive())
            .await
            .unwrap();
        let mut connection = pool.acquire().await.unwrap();

        let mut unknown = Vec::new();
        for pairs in [
            vec![],
            vec![("vertical", "retail")],
            vec![("vertical", "b2b")],
            vec![("brand", "x")],
            vec![("topic", "news")],
        ] {
            unknown.push(
                unknown_dimension_entry(&mut connection, ws, &dims(&pairs))
                    .await
                    .unwrap(),
            );
        }

        assert_eq!(
            unknown,
            [
                None,
                None,
                Some(("vertical".into(), "b2b".into())),
                Some(("brand".into(), "x".into())),
                Some(("topic".into(), "news".into())),
            ]
        );
    }
}
