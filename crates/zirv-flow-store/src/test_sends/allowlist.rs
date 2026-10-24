//! The workspace's test-send allowlist: addresses a test may go to besides the members'. The keyed
//! hash and a masked hint are stored, and the normalised address sealed with the keyring as an
//! `encrypted_secrets` row of purpose `test_send_address` whose id is the entry id.

use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{Keyring, Sealed, SecretBytes, SecretId, SecretPurpose, WorkspaceId};

use crate::StoreError;
use crate::secrets::{self, OpenError, SecretWriteError};

/// An allowlist entry as listed. It holds no address and no hash.
#[derive(Clone, Debug, Eq, PartialEq, FromRow)]
pub struct AllowlistEntry {
    pub id: Uuid,
    /// The first character, `***`, then `@domain`.
    pub display_hint: String,
    pub created_at: OffsetDateTime,
}

/// The hint shown for an address: its first character, `***`, then `@domain`. Text with no `@`
/// has no domain to show and is masked entirely.
pub fn masked_hint(address: &str) -> String {
    match address.split_once('@') {
        Some((local, domain)) => {
            let first = local.chars().next().map(String::from).unwrap_or_default();
            format!("{first}***@{domain}")
        }
        None => "***".to_owned(),
    }
}

/// Adds an entry: stores the sealed address as a secret of the entry's own id, then the entry.
/// `sealed` must be sealed with [`zirv_flow_core::secret_aad`] for this workspace, the entry id
/// and [`SecretPurpose::TestSendAddress`]. `None` when the hash is on the list already (nothing
/// is stored then). Run it in a transaction, as [`secrets::insert`] needs.
pub async fn add_allowlist_entry(
    connection: &mut PgConnection,
    allowed_key_ids: &[String],
    workspace: WorkspaceId,
    id: Uuid,
    address_hash: &[u8; 32],
    display_hint: &str,
    sealed: &Sealed,
) -> Result<Option<AllowlistEntry>, SecretWriteError> {
    if find_allowlist_entry(&mut *connection, workspace, address_hash)
        .await?
        .is_some()
    {
        return Ok(None);
    }
    secrets::insert(
        &mut *connection,
        allowed_key_ids,
        workspace,
        SecretPurpose::TestSendAddress,
        SecretId::from_uuid(id),
        sealed,
    )
    .await?;
    let entry = sqlx::query_as(
        "INSERT INTO test_send_allowlist (id, workspace_id, address_hash, display_hint) \
         VALUES ($1, $2, $3, $4) ON CONFLICT (workspace_id, address_hash) DO NOTHING \
         RETURNING id, display_hint, created_at",
    )
    .bind(id)
    .bind(workspace.as_uuid())
    .bind(address_hash.as_slice())
    .bind(display_hint)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(entry)
}

/// The workspace's entries, oldest first.
pub async fn list_allowlist(
    pool: &PgPool,
    workspace: WorkspaceId,
) -> Result<Vec<AllowlistEntry>, StoreError> {
    sqlx::query_as(
        "SELECT id, display_hint, created_at FROM test_send_allowlist \
         WHERE workspace_id = $1 ORDER BY created_at, id",
    )
    .bind(workspace.as_uuid())
    .fetch_all(pool)
    .await
    .map_err(StoreError::query)
}

/// Deletes an entry and its sealed address; false when the workspace has no such one.
pub async fn delete_allowlist_entry(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<bool, StoreError> {
    let done = sqlx::query(
        "DELETE FROM encrypted_secrets \
         WHERE workspace_id = $1 AND id = $2 AND purpose = 'test_send_address'",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(done.rows_affected() > 0)
}

/// Deletes the workspace's entries (and their sealed addresses) whose hash is one of `hashes`,
/// an erased person's; how many.
pub async fn delete_allowlist_by_hash(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    hashes: &[Vec<u8>],
) -> Result<u64, StoreError> {
    if hashes.is_empty() {
        return Ok(0);
    }
    let done = sqlx::query(
        "DELETE FROM encrypted_secrets WHERE workspace_id = $1 AND purpose = 'test_send_address' \
           AND id IN (SELECT id FROM test_send_allowlist \
                      WHERE workspace_id = $1 AND address_hash = ANY($2))",
    )
    .bind(workspace.as_uuid())
    .bind(hashes)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(done.rows_affected())
}

/// The id of the entry with this hash, when the workspace has one.
pub async fn find_allowlist_entry(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    address_hash: &[u8; 32],
) -> Result<Option<Uuid>, StoreError> {
    sqlx::query_scalar(
        "SELECT id FROM test_send_allowlist WHERE workspace_id = $1 AND address_hash = $2",
    )
    .bind(workspace.as_uuid())
    .bind(address_hash.as_slice())
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)
}

/// Opens the sealed address of the entry with this hash. `Ok(None)` when the entry is gone; an
/// entry the keyring cannot open is [`OpenError::Unopenable`].
pub async fn open_allowlist_address(
    connection: &mut PgConnection,
    keyring: &Keyring,
    workspace: WorkspaceId,
    address_hash: &[u8; 32],
) -> Result<Option<SecretBytes>, OpenError> {
    let Some(id) = find_allowlist_entry(&mut *connection, workspace, address_hash).await? else {
        return Ok(None);
    };
    match secrets::open_json(
        connection,
        keyring,
        workspace,
        SecretId::from_uuid(id),
        SecretPurpose::TestSendAddress,
    )
    .await
    {
        Ok(address) => Ok(Some(address)),
        Err(OpenError::NotFound) => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use zirv_flow_core::{MasterKey, Slug, WorkspaceName, seal, secret_aad};

    use super::*;

    #[test]
    fn a_hint_keeps_the_first_character_and_the_domain_only() {
        assert_eq!(masked_hint("ada@example.com"), "a***@example.com");
        assert_eq!(masked_hint("@example.com"), "***@example.com");
        assert_eq!(masked_hint("no-at-sign"), "***");
    }

    fn sealed_for(keys: &Keyring, ws: WorkspaceId, id: Uuid, address: &str) -> Sealed {
        seal(
            keys,
            address.as_bytes(),
            &secret_aad(ws, SecretId::from_uuid(id), SecretPurpose::TestSendAddress),
        )
    }

    async fn workspace(pool: &PgPool, keys: &Keyring) -> WorkspaceId {
        crate::migrate(pool).await.unwrap();
        secrets::insert_hmac_root_if_absent(pool, &seal(keys, b"root", b"aad"))
            .await
            .unwrap();
        crate::create_workspace(
            pool,
            &WorkspaceName::parse("one").unwrap(),
            &Slug::parse("one").unwrap(),
        )
        .await
        .unwrap()
        .id
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_entry_is_added_once_listed_opened_and_deleted_and_its_address_is_ciphertext(
        pool: PgPool,
    ) {
        let keys = Keyring::new(MasterKey::generate(), None).unwrap();
        let ws = workspace(&pool, &keys).await;
        let mut connection = pool.acquire().await.unwrap();
        let hash = [7_u8; 32];
        let ids = keys.key_ids();
        let id = Uuid::now_v7();
        let sealed = sealed_for(&keys, ws, id, "ak@x.test");

        let first =
            add_allowlist_entry(&mut connection, &ids, ws, id, &hash, "a***@x.test", &sealed)
                .await
                .unwrap()
                .expect("added");
        let again_id = Uuid::now_v7();
        let again = add_allowlist_entry(
            &mut connection,
            &ids,
            ws,
            again_id,
            &hash,
            "a***@x.test",
            &sealed_for(&keys, ws, again_id, "ak@x.test"),
        )
        .await
        .unwrap();

        assert!(again.is_none());
        assert_eq!(
            find_allowlist_entry(&mut connection, ws, &hash)
                .await
                .unwrap(),
            Some(first.id)
        );
        assert!(
            find_allowlist_entry(&mut connection, ws, &[8_u8; 32])
                .await
                .unwrap()
                .is_none()
        );
        let opened = open_allowlist_address(&mut connection, &keys, ws, &hash)
            .await
            .unwrap()
            .expect("opens");
        assert_eq!(opened.expose(), b"ak@x.test");
        let stored: Vec<u8> =
            sqlx::query_scalar("SELECT ciphertext FROM encrypted_secrets WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            !stored.windows(6).any(|window| window == b"x.test")
                && !stored.windows(3).any(|window| window == b"ak@"),
            "the address is ciphertext at rest"
        );
        let other = Keyring::new(MasterKey::generate(), None).unwrap();
        assert!(matches!(
            open_allowlist_address(&mut connection, &other, ws, &hash).await,
            Err(OpenError::Unopenable)
        ));
        assert_eq!(
            list_allowlist(&pool, ws).await.unwrap(),
            std::slice::from_ref(&first)
        );
        assert!(
            delete_allowlist_entry(&mut connection, ws, first.id)
                .await
                .unwrap()
        );
        assert!(
            !delete_allowlist_entry(&mut connection, ws, first.id)
                .await
                .unwrap()
        );
        assert!(
            open_allowlist_address(&mut connection, &keys, ws, &hash)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn entries_are_deleted_by_hash_with_their_sealed_address(pool: PgPool) {
        let keys = Keyring::new(MasterKey::generate(), None).unwrap();
        let ws = workspace(&pool, &keys).await;
        let mut connection = pool.acquire().await.unwrap();
        for (hash, address) in [([1_u8; 32], "a@x.test"), ([2_u8; 32], "b@x.test")] {
            let id = Uuid::now_v7();
            let sealed = sealed_for(&keys, ws, id, address);
            add_allowlist_entry(
                &mut connection,
                &keys.key_ids(),
                ws,
                id,
                &hash,
                "h",
                &sealed,
            )
            .await
            .unwrap()
            .unwrap();
        }

        let gone = delete_allowlist_by_hash(&mut connection, ws, &[vec![1; 32]])
            .await
            .unwrap();

        assert_eq!(gone, 1);
        assert_eq!(list_allowlist(&pool, ws).await.unwrap().len(), 1);
        let secrets: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM encrypted_secrets WHERE purpose = 'test_send_address'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(secrets, 1);
    }
}
