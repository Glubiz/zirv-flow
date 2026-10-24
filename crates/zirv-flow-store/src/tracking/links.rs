//! The message a token names and the links it may lead to.

use serde_json::Value;
use sqlx::PgExecutor;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::StoreError;

/// The message a tracking token names, as far as the tracking routes need it.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct TrackedMessage {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub person_id: Uuid,
    pub enrollment_id: Uuid,
    pub template_id: Uuid,
    /// How the message ended or where it is: `pending`, `sending`, `sent`, `skipped` or `failed`.
    pub status: String,
    pub topic_id: Option<Uuid>,
    /// The consent coordinates besides the topic: dimension key to value key.
    pub dimensions: Value,
    pub completed_at: Option<OffsetDateTime>,
}

impl TrackedMessage {
    pub fn workspace(&self) -> WorkspaceId {
        WorkspaceId::from_uuid(self.workspace_id)
    }
}

/// The message with this id, in whichever workspace it is. A token is signed, so the id is
/// trusted; nothing else is read from the request.
pub async fn find_tracked_message<'e>(
    executor: impl PgExecutor<'e>,
    id: Uuid,
) -> Result<Option<TrackedMessage>, StoreError> {
    sqlx::query_as(
        "SELECT id, workspace_id, person_id, enrollment_id, template_id, status, topic_id, \
                dimensions, completed_at \
         FROM messages WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// Records the links click tracking rewrote in a message, as `(index, URL)` pairs. Writing the
/// same index again replaces its URL, so a retried send leaves one row per link and the redirect
/// follows the message that went out last.
pub async fn record_message_links<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    message: Uuid,
    links: &[(u16, String)],
) -> Result<(), StoreError> {
    if links.is_empty() {
        return Ok(());
    }
    let indexes: Vec<i32> = links.iter().map(|(index, _)| i32::from(*index)).collect();
    let urls: Vec<&str> = links.iter().map(|(_, url)| url.as_str()).collect();
    sqlx::query(
        "INSERT INTO message_links (workspace_id, message_id, link_index, url) \
         SELECT $1, $2, t.link_index, t.url \
         FROM unnest($3::int4[], $4::text[]) AS t(link_index, url) \
         ON CONFLICT (message_id, link_index) DO UPDATE SET url = EXCLUDED.url",
    )
    .bind(workspace.as_uuid())
    .bind(message)
    .bind(&indexes)
    .bind(&urls)
    .execute(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// The URL recorded for link `index` of `message`; none when the message has no such link.
pub async fn find_message_link<'e>(
    executor: impl PgExecutor<'e>,
    message: Uuid,
    index: u16,
) -> Result<Option<String>, StoreError> {
    sqlx::query_scalar("SELECT url FROM message_links WHERE message_id = $1 AND link_index = $2")
        .bind(message)
        .bind(i32::from(index))
        .fetch_optional(executor)
        .await
        .map_err(StoreError::query)
}

#[cfg(test)]
pub(super) mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::flows::tests_support::workspace;

    /// A pending message of a new person, as `(person, message)`.
    pub(in crate::tracking) async fn message(pool: &PgPool, ws: WorkspaceId) -> (Uuid, Uuid) {
        let person = Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id, traits) VALUES ($1, $2, '{}')")
            .bind(person)
            .bind(ws.as_uuid())
            .execute(pool)
            .await
            .unwrap();
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO messages (id, workspace_id, person_id, enrollment_id, node_id, \
                template_id, destination_id, status) \
             VALUES ($1, $2, $3, gen_random_uuid(), 'n', gen_random_uuid(), gen_random_uuid(), \
                'pending')",
        )
        .bind(id)
        .bind(ws.as_uuid())
        .bind(person)
        .execute(pool)
        .await
        .unwrap();
        (person, id)
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn links_are_recorded_once_per_index_and_found_by_message_and_index(pool: PgPool) {
        let ws = workspace(&pool, "links").await;
        let (_, id) = message(&pool, ws).await;
        let (_, other) = message(&pool, ws).await;
        let links = vec![
            (0, "https://a.test/one".to_owned()),
            (1, "https://a.test/two".to_owned()),
        ];

        record_message_links(&pool, ws, id, &links).await.unwrap();
        record_message_links(&pool, ws, id, &links).await.unwrap();
        record_message_links(&pool, ws, id, &[(1, "https://a.test/three".to_owned())])
            .await
            .unwrap();
        record_message_links(&pool, ws, id, &[]).await.unwrap();

        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM message_links")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 2);
        assert_eq!(
            find_message_link(&pool, id, 0).await.unwrap().as_deref(),
            Some("https://a.test/one")
        );
        assert_eq!(
            find_message_link(&pool, id, 1).await.unwrap().as_deref(),
            Some("https://a.test/three")
        );
        assert_eq!(find_message_link(&pool, id, 2).await.unwrap(), None);
        assert_eq!(find_message_link(&pool, other, 0).await.unwrap(), None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_link_goes_with_its_message_and_needs_one(pool: PgPool) {
        let ws = workspace(&pool, "links2").await;
        let (_, id) = message(&pool, ws).await;
        record_message_links(&pool, ws, id, &[(0, "https://a.test/".to_owned())])
            .await
            .unwrap();

        let orphan = record_message_links(
            &pool,
            ws,
            Uuid::now_v7(),
            &[(0, "https://b.test/".to_owned())],
        )
        .await;
        assert!(orphan.is_err(), "a link needs its message");

        sqlx::query("DELETE FROM messages WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(find_message_link(&pool, id, 0).await.unwrap(), None);
        assert_eq!(find_tracked_message(&pool, id).await.unwrap(), None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_tracked_message_reads_its_coordinates(pool: PgPool) {
        let ws = workspace(&pool, "links3").await;
        let (_, id) = message(&pool, ws).await;

        let found = find_tracked_message(&pool, id).await.unwrap().unwrap();

        assert_eq!(found.workspace(), ws);
        assert_eq!(found.status, "pending");
        assert_eq!(found.completed_at, None);
    }
}
