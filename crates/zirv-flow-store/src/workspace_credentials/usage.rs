//! Where a workspace credential is used: the live sources and destinations that reference it.

use sqlx::PgExecutor;
use sqlx::prelude::FromRow;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::StoreError;

/// A source or destination that references a credential.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct CredentialUser {
    /// `source` or `destination`.
    pub kind: String,
    pub id: Uuid,
    pub name: String,
}

/// The live sources and destinations that use a credential, by kind then name. Every source
/// counts as live; an archived destination does not.
pub async fn credential_users<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    credential: Uuid,
) -> Result<Vec<CredentialUser>, StoreError> {
    sqlx::query_as(
        "SELECT kind, id, name FROM ( \
            SELECT 'source' AS kind, id, name FROM sources \
            WHERE workspace_id = $1 AND credential_id = $2 \
            UNION ALL \
            SELECT 'destination', id, name FROM destinations \
            WHERE workspace_id = $1 AND credential_id = $2 AND archived_at IS NULL) users \
         ORDER BY kind, lower(name), id",
    )
    .bind(workspace.as_uuid())
    .bind(credential)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// A live source or destination, with the credential it uses.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct CredentialUse {
    pub credential_id: Uuid,
    /// `source` or `destination`.
    pub kind: String,
    pub id: Uuid,
    pub name: String,
}

/// Every live source and destination of the workspace that uses a credential, by credential
/// then kind then name: one read for a whole list.
pub async fn credential_uses<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
) -> Result<Vec<CredentialUse>, StoreError> {
    sqlx::query_as(
        "SELECT credential_id, kind, id, name FROM (             SELECT credential_id, 'source' AS kind, id, name FROM sources             WHERE workspace_id = $1 AND credential_id IS NOT NULL             UNION ALL             SELECT credential_id, 'destination', id, name FROM destinations             WHERE workspace_id = $1 AND credential_id IS NOT NULL AND archived_at IS NULL) users          ORDER BY credential_id, kind, lower(name), id",
    )
    .bind(workspace.as_uuid())
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// How many live sources and destinations use each credential of the workspace that is used at
/// all.
pub async fn credential_use_counts<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
) -> Result<Vec<(Uuid, i64)>, StoreError> {
    sqlx::query_as(
        "SELECT credential_id, count(*) FROM ( \
            SELECT credential_id FROM sources \
            WHERE workspace_id = $1 AND credential_id IS NOT NULL \
            UNION ALL \
            SELECT credential_id FROM destinations \
            WHERE workspace_id = $1 AND credential_id IS NOT NULL AND archived_at IS NULL) users \
         GROUP BY credential_id",
    )
    .bind(workspace.as_uuid())
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;
    use crate::workspace_credentials::testing::{credential, workspace};
    use crate::{
        NewDestination, NewSource, archive_destination, create_destination, create_source,
    };

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn users_are_the_live_sources_and_destinations_of_the_credential_only(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let shared = credential(&pool, ws, "shared").await;
        let other = credential(&pool, ws, "other").await;
        let config = json!({});
        let source = create_source(
            &pool,
            ws,
            &NewSource {
                id: Uuid::now_v7(),
                kind: "fake",
                name: "Zeta source",
                config: &config,
                credential_id: Some(shared.id),
                schedule: None,
                created_by: None,
            },
        )
        .await
        .unwrap();
        let mut destinations = Vec::new();
        for (name, credential) in [
            ("Alpha", shared.id),
            ("Beta", shared.id),
            ("Gamma", other.id),
        ] {
            destinations.push(
                create_destination(
                    &pool,
                    ws,
                    &NewDestination {
                        id: Uuid::now_v7(),
                        kind: "fake",
                        name,
                        config: &config,
                        credential_id: Some(credential),
                        rate: 10,
                        burst: 10,
                        created_by: None,
                    },
                )
                .await
                .unwrap(),
            );
        }
        archive_destination(&pool, ws, destinations[1].id)
            .await
            .unwrap();

        let users = credential_users(&pool, ws, shared.id).await.unwrap();
        let uses = credential_uses(&pool, ws).await.unwrap();
        let counts: std::collections::BTreeMap<_, _> = credential_use_counts(&pool, ws)
            .await
            .unwrap()
            .into_iter()
            .collect();

        let seen: Vec<_> = users
            .iter()
            .map(|user| (user.kind.as_str(), user.name.as_str()))
            .collect();
        assert_eq!(seen, [("destination", "Alpha"), ("source", "Zeta source")]);
        assert_eq!(users[1].id, source.id);
        assert_eq!(counts.get(&shared.id), Some(&2));
        assert_eq!(counts.get(&other.id), Some(&1));
        assert_eq!(uses.len(), 3);
        assert_eq!(
            uses.iter()
                .filter(|used| used.credential_id == shared.id)
                .count(),
            2
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn another_workspace_sees_no_users(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let shared = credential(&pool, one, "shared").await;

        assert!(
            credential_users(&pool, two, shared.id)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(credential_use_counts(&pool, two).await.unwrap().is_empty());
    }
}
