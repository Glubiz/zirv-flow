//! The address a message is sent to (architecture 10.1, gate 2). It is read when the message is
//! sent and never stored.

use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::people::Identifier;

use crate::StoreError;

/// The email address of `person`, or none.
///
/// A registry trait flagged `channel_address` that holds a valid address wins (the first such
/// trait by key when several do). Otherwise the person's most recently seen email identity is
/// used. An address that does not normalize is not an address, whichever place it came from.
/// The result is normalized the way identifiers are, so a suppression hash taken from it matches
/// the one taken from the identity.
pub async fn read_send_address(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
) -> Result<Option<String>, StoreError> {
    let keys: Vec<String> = sqlx::query_scalar(
        "SELECT key FROM trait_definitions \
         WHERE workspace_id = $1 AND channel_address AND NOT archived ORDER BY key",
    )
    .bind(workspace.as_uuid())
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    if !keys.is_empty() {
        let traits: Option<Value> =
            sqlx::query_scalar("SELECT traits FROM people WHERE workspace_id = $1 AND id = $2")
                .bind(workspace.as_uuid())
                .bind(person)
                .fetch_optional(&mut *connection)
                .await
                .map_err(StoreError::query)?;
        let held = traits.as_ref().and_then(Value::as_object);
        let from_trait = keys
            .iter()
            .filter_map(|key| held?.get(key)?.as_str())
            .find_map(|value| email_of(value));
        if from_trait.is_some() {
            return Ok(from_trait);
        }
    }
    let identities: Vec<String> = sqlx::query_scalar(
        "SELECT value FROM identities \
         WHERE workspace_id = $1 AND person_id = $2 AND kind = 'email' \
         ORDER BY created_at DESC, id DESC",
    )
    .bind(workspace.as_uuid())
    .bind(person)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    Ok(identities.iter().find_map(|value| email_of(value)))
}

/// The normalized email address `value` stands for, if it is one.
fn email_of(value: &str) -> Option<String> {
    Identifier::parse("email", value)
        .ok()
        .map(|identifier| identifier.value().to_owned())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;
    use crate::create_workspace;

    async fn workspace(pool: &PgPool) -> WorkspaceId {
        let name = WorkspaceName::parse("one").unwrap();
        let slug = Slug::parse("one").unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    async fn person(pool: &PgPool, ws: WorkspaceId, traits: Value) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id, traits) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(ws.as_uuid())
            .bind(traits)
            .execute(pool)
            .await
            .unwrap();
        id
    }

    async fn identity(
        pool: &PgPool,
        ws: WorkspaceId,
        person: Uuid,
        kind: &str,
        value: &str,
        age: i32,
    ) {
        sqlx::query(
            "INSERT INTO identities (id, workspace_id, person_id, kind, value, created_at) \
             VALUES ($1, $2, $3, $4, $5, now() - $6 * interval '1 hour')",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .bind(person)
        .bind(kind)
        .bind(value)
        .bind(age)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn flag(
        pool: &PgPool,
        ws: WorkspaceId,
        key: &str,
        channel_address: bool,
        archived: bool,
    ) {
        sqlx::query(
            "INSERT INTO trait_definitions (workspace_id, key, type, channel_address, archived) \
             VALUES ($1, $2, 'string', $3, $4)",
        )
        .bind(ws.as_uuid())
        .bind(key)
        .bind(channel_address)
        .bind(archived)
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_flagged_trait_wins_then_the_newest_email_identity_then_nothing(pool: PgPool) {
        let ws = workspace(&pool).await;
        flag(&pool, ws, "work_email", true, false).await;
        flag(&pool, ws, "old_email", true, true).await;
        flag(&pool, ws, "nickname", false, false).await;
        let mut connection = pool.acquire().await.unwrap();

        let none = person(&pool, ws, json!({})).await;
        assert_eq!(
            read_send_address(&mut connection, ws, none).await.unwrap(),
            None
        );

        let by_identity = person(&pool, ws, json!({"nickname": "ada@trait.example"})).await;
        identity(&pool, ws, by_identity, "email", "old@identity.example", 5).await;
        identity(&pool, ws, by_identity, "email", "new@identity.example", 1).await;
        identity(&pool, ws, by_identity, "phone", "+4512345678", 0).await;
        assert_eq!(
            read_send_address(&mut connection, ws, by_identity)
                .await
                .unwrap()
                .as_deref(),
            Some("new@identity.example"),
            "the newest email identity; an unflagged trait and a phone are not addresses"
        );

        let by_trait = person(
            &pool,
            ws,
            json!({"work_email": " Ada@Trait.Example ", "old_email": "archived@trait.example"}),
        )
        .await;
        identity(&pool, ws, by_trait, "email", "id@identity.example", 1).await;
        assert_eq!(
            read_send_address(&mut connection, ws, by_trait)
                .await
                .unwrap()
                .as_deref(),
            Some("ada@trait.example"),
            "the flagged trait wins, normalized; an archived flag does not count"
        );

        let invalid = person(&pool, ws, json!({"work_email": "not an address"})).await;
        identity(&pool, ws, invalid, "email", "fallback@identity.example", 1).await;
        assert_eq!(
            read_send_address(&mut connection, ws, invalid)
                .await
                .unwrap()
                .as_deref(),
            Some("fallback@identity.example"),
            "a flagged trait that holds no valid address falls back to the identity"
        );
    }

    #[test]
    fn only_a_valid_address_is_an_address_and_it_is_normalized() {
        assert_eq!(
            email_of(" Ada@Example.COM "),
            Some("ada@example.com".to_owned())
        );
        for bad in ["", "ada", "ada@", "@example.com", "a b@example.com"] {
            assert_eq!(email_of(bad), None, "{bad:?}");
        }
    }
}
