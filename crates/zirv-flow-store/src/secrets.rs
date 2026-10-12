//! Sealed secrets (architecture section 16.2): the rows of `encrypted_secrets` and
//! `deployment_keys`. This module stores and returns [`Sealed`] values only. It never sees a
//! plaintext and never opens one; callers in `api` and `jobs` do that, right before use.

use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
use uuid::Uuid;
use zirv_flow_core::{Sealed, SecretId, SecretPurpose, WorkspaceId};

use crate::StoreError;

/// The name of the deployment key every HMAC key derives from.
pub const HMAC_ROOT: &str = "hmac_root";

/// A workspace secret as stored.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredSecret {
    pub purpose: SecretPurpose,
    pub sealed: Sealed,
}

/// One row that holds a wrapped data key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WrappedRow {
    /// A row of `encrypted_secrets`.
    Secret(SecretId),
    /// A row of `deployment_keys`.
    Deployment(String),
}

/// A row a master key still wraps, found for rotation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WrappedKey {
    pub row: WrappedRow,
    pub sealed: Sealed,
}

#[derive(FromRow)]
struct SecretRow {
    purpose: String,
    #[sqlx(flatten)]
    sealed: SealedRow,
}

#[derive(FromRow)]
struct IdRow {
    id: Uuid,
    #[sqlx(flatten)]
    sealed: SealedRow,
}

#[derive(FromRow)]
struct NameRow {
    name: String,
    #[sqlx(flatten)]
    sealed: SealedRow,
}

#[derive(FromRow)]
struct SealedRow {
    key_id: String,
    wrapped_key: Vec<u8>,
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
}

fn corrupt(what: &'static str) -> StoreError {
    StoreError::query(sqlx::Error::Decode(what.into()))
}

impl SealedRow {
    fn into_sealed(self) -> Result<Sealed, StoreError> {
        Ok(Sealed {
            key_id: self.key_id,
            wrapped_key: self.wrapped_key,
            nonce: self
                .nonce
                .try_into()
                .map_err(|_| corrupt("a secret nonce is not 24 bytes"))?,
            ciphertext: self.ciphertext,
        })
    }
}

/// Stores a new secret. The id is the one the sealing bound into the associated data.
pub async fn insert<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    purpose: SecretPurpose,
    id: SecretId,
    sealed: &Sealed,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO encrypted_secrets \
            (id, workspace_id, purpose, key_id, wrapped_key, nonce, ciphertext) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id.as_uuid())
    .bind(workspace.as_uuid())
    .bind(purpose.as_str())
    .bind(&sealed.key_id)
    .bind(&sealed.wrapped_key)
    .bind(sealed.nonce.as_slice())
    .bind(&sealed.ciphertext)
    .execute(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// A secret of this workspace; another workspace's id is not found.
pub async fn get<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: SecretId,
) -> Result<Option<StoredSecret>, StoreError> {
    let row: Option<SecretRow> = sqlx::query_as(
        "SELECT purpose, key_id, wrapped_key, nonce, ciphertext \
         FROM encrypted_secrets WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(id.as_uuid())
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    row.map(|row| {
        Ok(StoredSecret {
            purpose: SecretPurpose::parse(&row.purpose)
                .ok_or_else(|| corrupt("unknown purpose"))?,
            sealed: row.sealed.into_sealed()?,
        })
    })
    .transpose()
}

/// Replaces the sealed value of a secret of this workspace; false when there is none.
pub async fn replace<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: SecretId,
    sealed: &Sealed,
) -> Result<bool, StoreError> {
    let result = sqlx::query(
        "UPDATE encrypted_secrets \
         SET key_id = $3, wrapped_key = $4, nonce = $5, ciphertext = $6, updated_at = now() \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(id.as_uuid())
    .bind(&sealed.key_id)
    .bind(&sealed.wrapped_key)
    .bind(sealed.nonce.as_slice())
    .bind(&sealed.ciphertext)
    .execute(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(result.rows_affected() == 1)
}

/// Deletes a secret of this workspace; false when there is none.
pub async fn delete<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: SecretId,
) -> Result<bool, StoreError> {
    let result = sqlx::query("DELETE FROM encrypted_secrets WHERE workspace_id = $1 AND id = $2")
        .bind(workspace.as_uuid())
        .bind(id.as_uuid())
        .execute(executor)
        .await
        .map_err(StoreError::query)?;
    Ok(result.rows_affected() == 1)
}

/// The sealed HMAC root, if the deployment has one yet.
pub async fn hmac_root<'e>(executor: impl PgExecutor<'e>) -> Result<Option<Sealed>, StoreError> {
    let row: Option<SealedRow> = sqlx::query_as(
        "SELECT key_id, wrapped_key, nonce, ciphertext FROM deployment_keys WHERE name = $1",
    )
    .bind(HMAC_ROOT)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    row.map(SealedRow::into_sealed).transpose()
}

/// Stores the HMAC root unless one exists; true when this call stored it. Concurrent first
/// starts all call this, and exactly one wins.
pub async fn insert_hmac_root_if_absent<'e>(
    executor: impl PgExecutor<'e>,
    sealed: &Sealed,
) -> Result<bool, StoreError> {
    let result = sqlx::query(
        "INSERT INTO deployment_keys (name, key_id, wrapped_key, nonce, ciphertext) \
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT (name) DO NOTHING",
    )
    .bind(HMAC_ROOT)
    .bind(&sealed.key_id)
    .bind(&sealed.wrapped_key)
    .bind(sealed.nonce.as_slice())
    .bind(&sealed.ciphertext)
    .execute(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(result.rows_affected() == 1)
}

/// Locks and returns every row of both tables whose data key is not wrapped by `key_id`.
/// All of them are held in memory: rotation is meant for secrets in the thousands.
pub async fn rows_not_wrapped_by(
    connection: &mut PgConnection,
    key_id: &str,
) -> Result<Vec<WrappedKey>, StoreError> {
    let secrets: Vec<IdRow> = sqlx::query_as(
        "SELECT id, key_id, wrapped_key, nonce, ciphertext FROM encrypted_secrets \
         WHERE key_id <> $1 ORDER BY id FOR UPDATE",
    )
    .bind(key_id)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let deployment: Vec<NameRow> = sqlx::query_as(
        "SELECT name, key_id, wrapped_key, nonce, ciphertext FROM deployment_keys \
         WHERE key_id <> $1 ORDER BY name FOR UPDATE",
    )
    .bind(key_id)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;

    let mut rows = Vec::with_capacity(secrets.len() + deployment.len());
    for row in secrets {
        rows.push(WrappedKey {
            row: WrappedRow::Secret(SecretId::from_uuid(row.id)),
            sealed: row.sealed.into_sealed()?,
        });
    }
    for row in deployment {
        rows.push(WrappedKey {
            row: WrappedRow::Deployment(row.name),
            sealed: row.sealed.into_sealed()?,
        });
    }
    Ok(rows)
}

/// Points a row at a new wrapping key. The nonce and ciphertext are untouched.
pub async fn set_wrapped_key(
    connection: &mut PgConnection,
    row: &WrappedRow,
    key_id: &str,
    wrapped_key: &[u8],
) -> Result<(), StoreError> {
    let query = match row {
        WrappedRow::Secret(id) => sqlx::query(
            "UPDATE encrypted_secrets SET key_id = $2, wrapped_key = $3, updated_at = now() \
             WHERE id = $1",
        )
        .bind(id.as_uuid()),
        WrappedRow::Deployment(name) => sqlx::query(
            "UPDATE deployment_keys SET key_id = $2, wrapped_key = $3, updated_at = now() \
             WHERE name = $1",
        )
        .bind(name),
    };
    query
        .bind(key_id)
        .bind(wrapped_key)
        .execute(connection)
        .await
        .map_err(StoreError::query)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use zirv_flow_core::{
        HmacRoot, Keyring, MasterKey, Slug, WorkspaceName, deployment_key_aad, open, rewrap, seal,
        secret_aad,
    };

    use super::*;
    use crate::{begin, commit, create_workspace};

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    fn keyring() -> Keyring {
        Keyring::new(MasterKey::generate(), None).unwrap()
    }

    fn sealed_for(keys: &Keyring, ws: WorkspaceId, id: SecretId, plaintext: &[u8]) -> Sealed {
        seal(
            keys,
            plaintext,
            &secret_aad(ws, id, SecretPurpose::ConnectorCredential),
        )
    }

    #[test]
    fn a_stored_nonce_of_the_wrong_length_is_corrupt() {
        let row = SealedRow {
            key_id: "k".into(),
            wrapped_key: vec![1],
            nonce: vec![0; 23],
            ciphertext: vec![2],
        };

        assert!(row.into_sealed().is_err());
    }

    #[test]
    fn the_hmac_root_has_the_name_the_table_allows() {
        assert_eq!(HMAC_ROOT, "hmac_root");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_secret_is_inserted_read_replaced_and_deleted(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let keys = keyring();
        let id = SecretId::new();
        let sealed = sealed_for(&keys, ws, id, b"first");

        insert(&pool, ws, SecretPurpose::ConnectorCredential, id, &sealed)
            .await
            .unwrap();
        let stored = get(&pool, ws, id).await.unwrap().unwrap();
        assert_eq!(stored.purpose, SecretPurpose::ConnectorCredential);
        assert_eq!(stored.sealed, sealed);
        let aad = secret_aad(ws, id, stored.purpose);
        assert_eq!(
            open(&keys, &stored.sealed, &aad).unwrap().expose(),
            b"first"
        );

        let second = sealed_for(&keys, ws, id, b"second");
        assert!(replace(&pool, ws, id, &second).await.unwrap());
        let stored = get(&pool, ws, id).await.unwrap().unwrap();
        assert_eq!(
            open(&keys, &stored.sealed, &aad).unwrap().expose(),
            b"second"
        );

        assert!(delete(&pool, ws, id).await.unwrap());
        assert!(get(&pool, ws, id).await.unwrap().is_none());
        assert!(!delete(&pool, ws, id).await.unwrap());
        assert!(!replace(&pool, ws, id, &second).await.unwrap());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn another_workspace_cannot_read_replace_or_delete_a_secret(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let keys = keyring();
        let id = SecretId::new();
        let sealed = sealed_for(&keys, one, id, b"mine");
        insert(&pool, one, SecretPurpose::OAuthToken, id, &sealed)
            .await
            .unwrap();

        assert!(get(&pool, two, id).await.unwrap().is_none());
        assert!(
            !replace(&pool, two, id, &sealed_for(&keys, two, id, b"x"))
                .await
                .unwrap()
        );
        assert!(!delete(&pool, two, id).await.unwrap());

        let stored = get(&pool, one, id).await.unwrap().unwrap();
        assert_eq!(stored.sealed, sealed);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_ciphertext_moved_to_another_row_or_workspace_does_not_open(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let keys = keyring();
        let id = SecretId::new();
        let sealed = sealed_for(&keys, one, id, b"mine");
        // The same sealed value filed under another row and another workspace.
        let other = SecretId::new();
        insert(
            &pool,
            two,
            SecretPurpose::ConnectorCredential,
            other,
            &sealed,
        )
        .await
        .unwrap();

        let moved = get(&pool, two, other).await.unwrap().unwrap();

        let aad = secret_aad(two, other, moved.purpose);
        assert!(open(&keys, &moved.sealed, &aad).is_err());
        assert!(open(&keys, &moved.sealed, &secret_aad(one, other, moved.purpose)).is_err());
        assert!(open(&keys, &moved.sealed, &secret_aad(one, id, moved.purpose)).is_ok());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn deleting_a_workspace_deletes_its_secrets(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let keys = keyring();
        let id = SecretId::new();
        insert(
            &pool,
            ws,
            SecretPurpose::AiProviderKey,
            id,
            &sealed_for(&keys, ws, id, b"k"),
        )
        .await
        .unwrap();

        sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(ws.as_uuid())
            .execute(&pool)
            .await
            .unwrap();

        assert!(get(&pool, ws, id).await.unwrap().is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_database_refuses_an_unknown_purpose_and_a_short_nonce(pool: PgPool) {
        let ws = workspace(&pool, "one").await;

        let purpose = sqlx::query(
            "INSERT INTO encrypted_secrets \
                (id, workspace_id, purpose, key_id, wrapped_key, nonce, ciphertext) \
             VALUES ($1, $2, 'other', 'k', '\\x00', $3, '\\x00')",
        )
        .bind(Uuid::new_v4())
        .bind(ws.as_uuid())
        .bind(vec![0_u8; 24])
        .execute(&pool)
        .await;
        let nonce = sqlx::query(
            "INSERT INTO encrypted_secrets \
                (id, workspace_id, purpose, key_id, wrapped_key, nonce, ciphertext) \
             VALUES ($1, $2, 'oauth_token', 'k', '\\x00', $3, '\\x00')",
        )
        .bind(Uuid::new_v4())
        .bind(ws.as_uuid())
        .bind(vec![0_u8; 12])
        .execute(&pool)
        .await;

        assert!(purpose.is_err());
        assert!(nonce.is_err());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_hmac_root_is_stored_once_and_the_first_writer_wins(pool: PgPool) {
        let keys = keyring();
        let aad = deployment_key_aad(HMAC_ROOT);
        assert!(hmac_root(&pool).await.unwrap().is_none());
        let first = seal(&keys, b"first-root", &aad);
        let second = seal(&keys, b"second-root", &aad);

        assert!(insert_hmac_root_if_absent(&pool, &first).await.unwrap());
        assert!(!insert_hmac_root_if_absent(&pool, &second).await.unwrap());

        assert_eq!(hmac_root(&pool).await.unwrap().unwrap(), first);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn concurrent_first_starts_leave_one_root_that_every_task_opens(pool: PgPool) {
        let keys = std::sync::Arc::new(keyring());
        let aad = deployment_key_aad(HMAC_ROOT);

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let pool = pool.clone();
            let keys = std::sync::Arc::clone(&keys);
            let aad = aad.clone();
            tasks.push(tokio::spawn(async move {
                // Each start proposes a root of its own; only one is kept.
                let candidate = HmacRoot::bootstrap_from(&MasterKey::generate());
                let sealed = seal(&keys, candidate.to_secret_bytes().expose(), &aad);
                insert_hmac_root_if_absent(&pool, &sealed).await.unwrap();
                let stored = hmac_root(&pool).await.unwrap().unwrap();
                open(&keys, &stored, &aad).unwrap().expose().to_vec()
            }));
        }
        let mut roots = Vec::new();
        for task in tasks {
            roots.push(task.await.unwrap());
        }

        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM deployment_keys")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1);
        assert!(roots.iter().all(|root| *root == roots[0]));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn rows_not_wrapped_by_the_current_key_are_found_and_rewrapped(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let old = Keyring::new(MasterKey::generate(), None).unwrap();
        let id = SecretId::new();
        let sealed = sealed_for(&old, ws, id, b"secret");
        insert(&pool, ws, SecretPurpose::ConnectorCredential, id, &sealed)
            .await
            .unwrap();
        let root = seal(&old, b"root", &deployment_key_aad(HMAC_ROOT));
        insert_hmac_root_if_absent(&pool, &root).await.unwrap();
        let new_key = MasterKey::generate();
        let rotated = Keyring::new(
            new_key,
            Some(MasterKey::parse(&old.current().to_base64()).unwrap()),
        )
        .unwrap();
        let new_id = rotated.current().key_id();

        let mut tx = begin(&pool).await.unwrap();
        let found = rows_not_wrapped_by(&mut tx, &new_id).await.unwrap();
        assert_eq!(found.len(), 2);
        for found in &found {
            let wrapped = rewrap(&rotated, &found.sealed).unwrap();
            set_wrapped_key(&mut tx, &found.row, &wrapped.key_id, &wrapped.wrapped_key)
                .await
                .unwrap();
        }
        assert!(
            rows_not_wrapped_by(&mut tx, &new_id)
                .await
                .unwrap()
                .is_empty()
        );
        commit(tx).await.unwrap();

        let stored = get(&pool, ws, id).await.unwrap().unwrap();
        assert_eq!(stored.sealed.key_id, new_id);
        assert_eq!(stored.sealed.nonce, sealed.nonce);
        assert_eq!(stored.sealed.ciphertext, sealed.ciphertext);
        let aad = secret_aad(ws, id, stored.purpose);
        let new_only = Keyring::new(
            MasterKey::parse(&rotated.current().to_base64()).unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(
            open(&new_only, &stored.sealed, &aad).unwrap().expose(),
            b"secret"
        );
        assert_eq!(hmac_root(&pool).await.unwrap().unwrap().key_id, new_id);
    }

    /// The test of the dump: with a stored credential, no row of any table, read as text in any
    /// bytea encoding, holds the plaintext in UTF-8 or in hex.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn no_table_holds_the_plaintext_of_a_stored_secret(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let keys = keyring();
        let marker = format!("zirv-marker-{}", Uuid::new_v4());
        let id = SecretId::new();
        insert(
            &pool,
            ws,
            SecretPurpose::ConnectorCredential,
            id,
            &sealed_for(&keys, ws, id, marker.as_bytes()),
        )
        .await
        .unwrap();
        let root = seal(&keys, marker.as_bytes(), &deployment_key_aad(HMAC_ROOT));
        insert_hmac_root_if_absent(&pool, &root).await.unwrap();
        let hex: String = marker.bytes().map(|byte| format!("{byte:02x}")).collect();

        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT tablename FROM pg_tables WHERE schemaname = current_schema()",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(tables.iter().any(|table| table == "encrypted_secrets"));
        let mut connection = pool.acquire().await.unwrap();
        let mut dumped = 0;
        for encoding in ["hex", "escape"] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "SET bytea_output = '{encoding}'"
            )))
            .execute(&mut *connection)
            .await
            .unwrap();
            for table in &tables {
                let text: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "SELECT string_agg(t::text, E'\\n') FROM \"{table}\" t"
                )))
                .fetch_one(&mut *connection)
                .await
                .unwrap();
                let text = text.unwrap_or_default();
                dumped += text.len();
                assert!(
                    !text.contains(&marker),
                    "{table} holds the plaintext ({encoding})"
                );
                assert!(
                    !text.contains(&hex),
                    "{table} holds the plaintext in hex ({encoding})"
                );
            }
        }
        assert!(dumped > 0);
    }
}
