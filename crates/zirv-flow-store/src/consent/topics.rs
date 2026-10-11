//! The topic catalog of a workspace (architecture section 6): what a person can be
//! subscribed to, and the policy that decides what silence means.

use sqlx::PgExecutor;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::consent::{Channel, TopicPolicy};

use crate::StoreError;

macro_rules! topic_columns {
    () => {
        "id, workspace_id, key, name, description, channel, policy, archived_at, created_at, \
         updated_at"
    };
}

/// A topic as stored.
#[derive(Clone, Debug, PartialEq)]
pub struct Topic {
    pub id: Uuid,
    pub workspace_id: WorkspaceId,
    pub key: String,
    pub name: String,
    pub description: String,
    pub channel: Channel,
    pub policy: TopicPolicy,
    pub archived_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// A topic to create. The key is already validated.
#[derive(Clone, Debug)]
pub struct NewTopic {
    pub key: String,
    pub name: String,
    pub description: String,
    pub channel: Channel,
    pub policy: TopicPolicy,
}

/// The mutable fields of a topic; `None` leaves a field as it is. The key and the channel
/// never change.
#[derive(Clone, Debug, Default)]
pub struct TopicUpdate {
    pub name: Option<String>,
    pub description: Option<String>,
    pub policy: Option<TopicPolicy>,
    pub archived: Option<bool>,
}

#[derive(FromRow)]
struct TopicRow {
    id: Uuid,
    workspace_id: Uuid,
    key: String,
    name: String,
    description: String,
    channel: String,
    policy: String,
    archived_at: Option<OffsetDateTime>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

fn corrupt(what: &'static str) -> StoreError {
    StoreError::query(sqlx::Error::Decode(what.into()))
}

impl TopicRow {
    fn into_topic(self) -> Result<Topic, StoreError> {
        Ok(Topic {
            id: self.id,
            workspace_id: WorkspaceId::from_uuid(self.workspace_id),
            key: self.key,
            name: self.name,
            description: self.description,
            channel: self
                .channel
                .parse()
                .map_err(|_| corrupt("unknown channel"))?,
            policy: self.policy.parse().map_err(|_| corrupt("unknown policy"))?,
            archived_at: self.archived_at,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

/// The workspace's topics ordered by key. Archived ones are left out unless asked for.
pub async fn list_topics<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    include_archived: bool,
) -> Result<Vec<Topic>, StoreError> {
    let rows: Vec<TopicRow> = sqlx::query_as(concat!(
        "SELECT ",
        topic_columns!(),
        " FROM topics WHERE workspace_id = $1 AND ($2 OR archived_at IS NULL) ORDER BY key"
    ))
    .bind(workspace.as_uuid())
    .bind(include_archived)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    rows.into_iter().map(TopicRow::into_topic).collect()
}

pub async fn find_topic<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<Topic>, StoreError> {
    let row: Option<TopicRow> = sqlx::query_as(concat!(
        "SELECT ",
        topic_columns!(),
        " FROM topics WHERE workspace_id = $1 AND id = $2"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    row.map(TopicRow::into_topic).transpose()
}

pub async fn find_topic_by_key<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    key: &str,
) -> Result<Option<Topic>, StoreError> {
    let row: Option<TopicRow> = sqlx::query_as(concat!(
        "SELECT ",
        topic_columns!(),
        " FROM topics WHERE workspace_id = $1 AND key = $2"
    ))
    .bind(workspace.as_uuid())
    .bind(key)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    row.map(TopicRow::into_topic).transpose()
}

/// Creates a topic. A key that exists, archived or not, is a conflict.
pub async fn create_topic<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    new: &NewTopic,
) -> Result<Topic, StoreError> {
    let row: TopicRow = sqlx::query_as(concat!(
        "INSERT INTO topics (id, workspace_id, key, name, description, channel, policy) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING ",
        topic_columns!()
    ))
    .bind(Uuid::now_v7())
    .bind(workspace.as_uuid())
    .bind(&new.key)
    .bind(&new.name)
    .bind(&new.description)
    .bind(new.channel.as_str())
    .bind(new.policy.as_str())
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)?;
    row.into_topic()
}

/// Applies the given fields. `None` when the workspace has no such topic. Archiving keeps the
/// time of the first archive; un-archiving clears it.
pub async fn update_topic<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
    update: &TopicUpdate,
) -> Result<Option<Topic>, StoreError> {
    let row: Option<TopicRow> = sqlx::query_as(concat!(
        "UPDATE topics SET \
            name = COALESCE($3, name), \
            description = COALESCE($4, description), \
            policy = COALESCE($5, policy), \
            archived_at = CASE WHEN $6::boolean IS NULL THEN archived_at \
                               WHEN $6 THEN COALESCE(archived_at, now()) ELSE NULL END, \
            updated_at = now() \
         WHERE workspace_id = $1 AND id = $2 RETURNING ",
        topic_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(update.name.as_deref())
    .bind(update.description.as_deref())
    .bind(update.policy.map(TopicPolicy::as_str))
    .bind(update.archived)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    row.map(TopicRow::into_topic).transpose()
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;
    use crate::create_workspace;

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    fn new_topic(key: &str, policy: TopicPolicy) -> NewTopic {
        NewTopic {
            key: key.into(),
            name: format!("Name of {key}"),
            description: String::new(),
            channel: Channel::Email,
            policy,
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_topic_is_created_found_updated_archived_and_listed(pool: PgPool) {
        let ws = workspace(&pool, "one").await;

        let news = create_topic(&pool, ws, &new_topic("news", TopicPolicy::OptOut))
            .await
            .unwrap();
        create_topic(&pool, ws, &new_topic("alerts", TopicPolicy::OptIn))
            .await
            .unwrap();

        assert_eq!(
            find_topic(&pool, ws, news.id).await.unwrap(),
            Some(news.clone())
        );
        assert_eq!(
            find_topic_by_key(&pool, ws, "news").await.unwrap(),
            Some(news.clone())
        );
        let keys = |topics: Vec<Topic>| topics.into_iter().map(|t| t.key).collect::<Vec<_>>();
        assert_eq!(
            keys(list_topics(&pool, ws, false).await.unwrap()),
            ["alerts", "news"]
        );

        let updated = update_topic(
            &pool,
            ws,
            news.id,
            &TopicUpdate {
                name: Some("Renamed".into()),
                policy: Some(TopicPolicy::OptIn),
                archived: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updated.name, "Renamed");
        assert_eq!(updated.policy, TopicPolicy::OptIn);
        assert!(updated.archived_at.is_some());
        assert_eq!(
            keys(list_topics(&pool, ws, false).await.unwrap()),
            ["alerts"]
        );
        assert_eq!(
            keys(list_topics(&pool, ws, true).await.unwrap()),
            ["alerts", "news"]
        );

        let first_archive = updated.archived_at;
        let again = update_topic(
            &pool,
            ws,
            news.id,
            &TopicUpdate {
                archived: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(again.archived_at, first_archive);
        let restored = update_topic(
            &pool,
            ws,
            news.id,
            &TopicUpdate {
                archived: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert!(restored.archived_at.is_none());
        assert_eq!(restored.name, "Renamed");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_key_is_unique_per_workspace_and_a_topic_is_private_to_its_workspace(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let topic = create_topic(&pool, one, &new_topic("news", TopicPolicy::OptOut))
            .await
            .unwrap();

        let duplicate = create_topic(&pool, one, &new_topic("news", TopicPolicy::OptIn)).await;
        let in_two = create_topic(&pool, two, &new_topic("news", TopicPolicy::OptIn))
            .await
            .unwrap();

        assert!(duplicate.unwrap_err().is_conflict());
        assert_ne!(in_two.id, topic.id);
        assert!(find_topic(&pool, two, topic.id).await.unwrap().is_none());
        assert!(
            find_topic_by_key(&pool, two, "other")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            update_topic(&pool, two, topic.id, &TopicUpdate::default())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_database_refuses_a_malformed_key(pool: PgPool) {
        let ws = workspace(&pool, "one").await;

        let refused = create_topic(&pool, ws, &new_topic("Not A Key", TopicPolicy::OptIn)).await;

        assert!(refused.is_err());
    }
}
