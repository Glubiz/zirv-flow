//! Creating, reading, changing, rotating and archiving workspace credentials. Every function is
//! scoped to one workspace. The sealed secret is created by the caller (the admin operation seals
//! it) in the same transaction; this module only links, swaps and unlinks it.

use serde_json::Value;
use sqlx::PgConnection;
use sqlx::PgExecutor;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{SecretId, UserId, WorkspaceId};

use super::usage::{CredentialUser, credential_users};
use crate::StoreError;

/// A workspace credential as stored: the non-secret values, hints and the reference to the sealed
/// secret. Never a secret value.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct WorkspaceCredential {
    pub id: Uuid,
    pub key: String,
    pub name: String,
    /// A built-in kind, `custom` or `imported`.
    pub kind: String,
    pub schema_version: i32,
    /// The schema of a `custom` credential.
    pub schema: Option<Value>,
    /// The non-secret values.
    pub fields: Value,
    pub secret_id: Uuid,
    /// Per secret field, its last four characters or an empty string.
    pub hints: Value,
    pub fingerprint: Option<String>,
    pub rotated_at: OffsetDateTime,
    pub rotated_by: Option<Uuid>,
    pub last_tested_at: Option<OffsetDateTime>,
    /// `ok` or `failed`.
    pub last_test_outcome: Option<String>,
    pub archived_at: Option<OffsetDateTime>,
    pub created_by: Option<Uuid>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// The columns of [`WorkspaceCredential`].
macro_rules! credential_columns {
    () => {
        "id, key, name, kind, schema_version, schema, fields, secret_id, hints, fingerprint, \
         rotated_at, rotated_by, last_tested_at, last_test_outcome, archived_at, created_by, \
         created_at, updated_at"
    };
}

/// A credential to create. The secret named by `secret_id` must already exist in the same
/// transaction.
#[derive(Debug)]
pub struct NewWorkspaceCredential<'a> {
    pub id: Uuid,
    pub key: &'a str,
    pub name: &'a str,
    pub kind: &'a str,
    pub schema: Option<&'a Value>,
    pub fields: &'a Value,
    pub secret_id: SecretId,
    pub hints: &'a Value,
    pub fingerprint: Option<&'a str>,
    pub created_by: Option<UserId>,
}

/// The non-secret parts of a credential that can change; `None` leaves a part as it is.
#[derive(Debug, Default)]
pub struct CredentialUpdate<'a> {
    pub name: Option<&'a str>,
    /// Replaces the whole object of non-secret values.
    pub fields: Option<&'a Value>,
}

/// Creates a credential. A key or a name already used by a live credential of the workspace is a
/// conflict ([`StoreError::is_conflict`]).
pub async fn create_credential<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    new: &NewWorkspaceCredential<'_>,
) -> Result<WorkspaceCredential, StoreError> {
    sqlx::query_as(concat!(
        "INSERT INTO credentials (id, workspace_id, key, name, kind, schema, fields, secret_id, \
            hints, fingerprint, rotated_by, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $11) RETURNING ",
        credential_columns!()
    ))
    .bind(new.id)
    .bind(workspace.as_uuid())
    .bind(new.key)
    .bind(new.name)
    .bind(new.kind)
    .bind(new.schema)
    .bind(new.fields)
    .bind(new.secret_id.as_uuid())
    .bind(new.hints)
    .bind(new.fingerprint)
    .bind(new.created_by.map(|user| user.as_uuid()))
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)
}

/// The workspace's credentials, ordered by name; archived ones only when asked for.
pub async fn list_credentials<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    include_archived: bool,
) -> Result<Vec<WorkspaceCredential>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        credential_columns!(),
        " FROM credentials WHERE workspace_id = $1 AND ($2 OR archived_at IS NULL) \
          ORDER BY lower(name), id"
    ))
    .bind(workspace.as_uuid())
    .bind(include_archived)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// One credential, archived or not; another workspace's id is not found.
pub async fn find_credential<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<WorkspaceCredential>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        credential_columns!(),
        " FROM credentials WHERE workspace_id = $1 AND id = $2"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// As [`find_credential`], and locks the row until the transaction ends, so a rotation, a change
/// and an archive of one credential run one after the other.
pub async fn find_credential_for_update(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<WorkspaceCredential>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        credential_columns!(),
        " FROM credentials WHERE workspace_id = $1 AND id = $2 FOR UPDATE"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)
}

/// Reads a live credential and locks it `FOR SHARE` until the transaction ends. A source or
/// destination takes this before it links the credential: an archive waits for it and then sees
/// the new reference, and a credential archived earlier is not found, so an archived credential
/// is never referenced. `None` for an archived credential and for another workspace's.
pub async fn lock_credential_for_use(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<WorkspaceCredential>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        credential_columns!(),
        " FROM credentials WHERE workspace_id = $1 AND id = $2 AND archived_at IS NULL FOR SHARE"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)
}

/// Changes the name or the non-secret values of a live credential. `None` when there is none; a
/// name taken by another live credential is a conflict.
pub async fn update_credential<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
    update: &CredentialUpdate<'_>,
) -> Result<Option<WorkspaceCredential>, StoreError> {
    sqlx::query_as(concat!(
        "UPDATE credentials SET name = COALESCE($3, name), fields = COALESCE($4, fields), \
            updated_at = now() \
         WHERE workspace_id = $1 AND id = $2 AND archived_at IS NULL RETURNING ",
        credential_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(update.name)
    .bind(update.fields)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// Points a live credential at a new sealed secret and returns the secret it replaced; `None`
/// when there is no live credential. The caller created the new secret and, in the same
/// transaction, deletes the old row ([`secrets::delete`]), so a rotation is one atomic swap and no
/// reader sees the old value after the commit.
pub async fn swap_credential_secret(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    new_secret: SecretId,
    hints: &Value,
    fingerprint: Option<&str>,
    rotated_by: Option<UserId>,
) -> Result<Option<SecretId>, StoreError> {
    let old: Option<Uuid> = sqlx::query_scalar(
        "WITH old AS ( \
            SELECT secret_id FROM credentials \
            WHERE workspace_id = $1 AND id = $2 AND archived_at IS NULL FOR UPDATE), \
         swapped AS ( \
            UPDATE credentials SET secret_id = $3, hints = $4, fingerprint = $5, \
                rotated_at = now(), rotated_by = $6, updated_at = now() \
            WHERE workspace_id = $1 AND id = $2 AND archived_at IS NULL RETURNING id) \
         SELECT old.secret_id FROM old, swapped",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(new_secret.as_uuid())
    .bind(hints)
    .bind(fingerprint)
    .bind(rotated_by.map(|user| user.as_uuid()))
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    Ok(old.map(SecretId::from_uuid))
}

/// Records the outcome of a credential test. False when there is no live credential.
pub async fn record_credential_test<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
    succeeded: bool,
) -> Result<bool, StoreError> {
    let result = sqlx::query(
        "UPDATE credentials SET last_tested_at = now(), last_test_outcome = $3 \
         WHERE workspace_id = $1 AND id = $2 AND archived_at IS NULL",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(if succeeded { "ok" } else { "failed" })
    .execute(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(result.rows_affected() == 1)
}

/// Links an already sealed secret as an `imported` credential named `name`, the way migration 0047
/// did for the credentials sources and destinations had sealed themselves: nothing is re-sealed,
/// every field is secret and fully masked. For tests that plant a sealed secret first.
#[cfg(any(test, feature = "test-support"))]
pub async fn import_secret_as_credential<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    name: &str,
    secret_id: SecretId,
    field_names: &[&str],
) -> Result<WorkspaceCredential, StoreError> {
    let hints: serde_json::Map<String, Value> = field_names
        .iter()
        .map(|field| ((*field).to_owned(), Value::String(String::new())))
        .collect();
    let id = Uuid::now_v7();
    create_credential(
        executor,
        workspace,
        &NewWorkspaceCredential {
            id,
            key: &format!("imported-{}", id.simple()),
            name,
            kind: zirv_flow_core::credentials::IMPORTED_KIND,
            schema: None,
            fields: &Value::Object(serde_json::Map::new()),
            secret_id,
            hints: &Value::Object(hints),
            fingerprint: None,
            created_by: None,
        },
    )
    .await
}

/// What archiving a credential did.
#[derive(Debug, Eq, PartialEq)]
pub enum ArchiveOutcome {
    Archived(Box<WorkspaceCredential>),
    /// Live sources or destinations still use it; nothing changed.
    InUse(Vec<CredentialUser>),
    AlreadyArchived,
    NotFound,
}

/// Archives a credential unless a live source or destination references it. The row is locked
/// first, so a reference taken concurrently ([`lock_credential_for_use`]) is either seen here or
/// refused there. Its key and name are free again; the sealed secret stays for the record.
pub async fn archive_credential(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<ArchiveOutcome, StoreError> {
    let Some(found) = find_credential_for_update(connection, workspace, id).await? else {
        return Ok(ArchiveOutcome::NotFound);
    };
    if found.archived_at.is_some() {
        return Ok(ArchiveOutcome::AlreadyArchived);
    }
    let users = credential_users(&mut *connection, workspace, id).await?;
    if !users.is_empty() {
        return Ok(ArchiveOutcome::InUse(users));
    }
    let archived: WorkspaceCredential = sqlx::query_as(concat!(
        "UPDATE credentials SET archived_at = now(), updated_at = now() \
         WHERE workspace_id = $1 AND id = $2 RETURNING ",
        credential_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    Ok(ArchiveOutcome::Archived(Box::new(archived)))
}

#[cfg(test)]
pub(crate) mod testing {
    use serde_json::json;
    use sqlx::PgPool;
    use zirv_flow_core::{
        Keyring, MasterKey, SecretPurpose, Slug, WorkspaceName, seal, secret_aad,
    };

    use super::*;
    use crate::{create_workspace, secrets};

    pub(crate) async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    /// Plants a sealed secret of `purpose` holding `plaintext` and returns its id.
    pub(crate) async fn secret(
        pool: &PgPool,
        keys: &Keyring,
        ws: WorkspaceId,
        purpose: SecretPurpose,
        plaintext: &[u8],
    ) -> SecretId {
        let id = SecretId::new();
        let sealed = seal(keys, plaintext, &secret_aad(ws, id, purpose));
        secrets::insert_unchecked(pool, ws, purpose, id, &sealed)
            .await
            .unwrap();
        id
    }

    /// Waits until some other backend of this database is blocked on a lock, polling for up to
    /// five seconds, and fails with a clear message when none ever is, so a test knows a call
    /// really waited for the transaction it holds open.
    pub(crate) async fn wait_until_a_backend_waits_on_a_lock(pool: &PgPool) {
        for _ in 0..100 {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE datname = current_database() AND wait_event_type = 'Lock' \
                   AND pid <> pg_backend_pid()",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            if waiting > 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("no backend ever waited on a lock: the call did not block on the held transaction");
    }

    /// A live `api_key` credential named `key`, sealed under a throwaway key.
    pub(crate) async fn credential(
        pool: &PgPool,
        ws: WorkspaceId,
        key: &str,
    ) -> WorkspaceCredential {
        credential_with_fields(pool, ws, key, &["api_key"]).await
    }

    /// As [`credential`], holding secret fields with these names.
    pub(crate) async fn credential_with_fields(
        pool: &PgPool,
        ws: WorkspaceId,
        key: &str,
        names: &[&str],
    ) -> WorkspaceCredential {
        let hints: serde_json::Map<String, Value> = names
            .iter()
            .map(|name| ((*name).to_owned(), json!("")))
            .collect();
        let keys = Keyring::new(MasterKey::generate(), None).unwrap();
        let secret_id = secret(
            pool,
            &keys,
            ws,
            SecretPurpose::WorkspaceCredential,
            br#"{"api_key":"k"}"#,
        )
        .await;
        create_credential(
            pool,
            ws,
            &NewWorkspaceCredential {
                id: Uuid::now_v7(),
                key,
                name: key,
                kind: "api_key",
                schema: None,
                fields: &json!({}),
                secret_id,
                hints: &Value::Object(hints),
                fingerprint: None,
                created_by: None,
            },
        )
        .await
        .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use zirv_flow_core::{Keyring, MasterKey, SecretPurpose, seal, secret_aad};

    use super::testing::{credential, secret, wait_until_a_backend_waits_on_a_lock, workspace};
    use super::*;
    use crate::{NewSource, begin, commit, create_source, secrets};

    fn new<'a>(
        key: &'a str,
        name: &'a str,
        secret_id: SecretId,
        fields: &'a Value,
        hints: &'a Value,
    ) -> NewWorkspaceCredential<'a> {
        NewWorkspaceCredential {
            id: Uuid::now_v7(),
            key,
            name,
            kind: "smtp",
            schema: None,
            fields,
            secret_id,
            hints,
            fingerprint: Some("ab"),
            created_by: None,
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_credential_is_created_read_listed_updated_and_archived(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let keys = Keyring::new(MasterKey::generate(), None).unwrap();
        let secret_id = secret(&pool, &keys, ws, SecretPurpose::WorkspaceCredential, b"{}").await;
        let fields = json!({"host": "h", "port": 587});
        let hints = json!({"password": "wxyz"});

        let created =
            create_credential(&pool, ws, &new("mail", "Mail", secret_id, &fields, &hints))
                .await
                .unwrap();
        credential(&pool, ws, "alpha").await;

        assert_eq!(created.fields, fields);
        assert_eq!(created.hints, hints);
        assert_eq!(created.secret_id, secret_id.as_uuid());
        assert!(created.archived_at.is_none() && created.last_tested_at.is_none());
        assert_eq!(
            find_credential(&pool, ws, created.id)
                .await
                .unwrap()
                .unwrap(),
            created
        );
        let names: Vec<_> = list_credentials(&pool, ws, false)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["alpha", "Mail"]);

        let more = json!({"host": "other"});
        let updated = update_credential(
            &pool,
            ws,
            created.id,
            &CredentialUpdate {
                name: Some("Mail 2"),
                fields: Some(&more),
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!((updated.name.as_str(), &updated.fields), ("Mail 2", &more));
        assert_eq!(updated.secret_id, created.secret_id);

        assert!(
            record_credential_test(&pool, ws, created.id, true)
                .await
                .unwrap()
        );
        let tested = find_credential(&pool, ws, created.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tested.last_test_outcome.as_deref(), Some("ok"));
        assert!(tested.last_tested_at.is_some());
        assert!(
            record_credential_test(&pool, ws, created.id, false)
                .await
                .unwrap()
        );

        let mut tx = begin(&pool).await.unwrap();
        let outcome = archive_credential(&mut tx, ws, created.id).await.unwrap();
        commit(tx).await.unwrap();
        assert!(matches!(outcome, ArchiveOutcome::Archived(c) if c.archived_at.is_some()));
        assert_eq!(list_credentials(&pool, ws, false).await.unwrap().len(), 1);
        assert_eq!(list_credentials(&pool, ws, true).await.unwrap().len(), 2);
        assert!(
            update_credential(&pool, ws, created.id, &CredentialUpdate::default())
                .await
                .unwrap()
                .is_none()
        );
        let mut tx = begin(&pool).await.unwrap();
        assert_eq!(
            archive_credential(&mut tx, ws, created.id).await.unwrap(),
            ArchiveOutcome::AlreadyArchived
        );
        assert_eq!(
            archive_credential(&mut tx, ws, Uuid::now_v7())
                .await
                .unwrap(),
            ArchiveOutcome::NotFound
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_key_and_a_name_are_unique_among_live_credentials_per_workspace(pool: PgPool) {
        let (one, two) = (workspace(&pool, "one").await, workspace(&pool, "two").await);
        let first = credential(&pool, one, "mail").await;
        let keys = Keyring::new(MasterKey::generate(), None).unwrap();
        let (fields, hints) = (json!({}), json!({}));

        let same_key = secret(&pool, &keys, one, SecretPurpose::WorkspaceCredential, b"{}").await;
        let duplicate_key =
            create_credential(&pool, one, &new("mail", "Other", same_key, &fields, &hints)).await;
        let same_name = secret(&pool, &keys, one, SecretPurpose::WorkspaceCredential, b"{}").await;
        let duplicate_name = create_credential(
            &pool,
            one,
            &new("other", "MAIL", same_name, &fields, &hints),
        )
        .await;
        let elsewhere = secret(&pool, &keys, two, SecretPurpose::WorkspaceCredential, b"{}").await;
        let other_workspace =
            create_credential(&pool, two, &new("mail", "mail", elsewhere, &fields, &hints)).await;

        assert!(duplicate_key.unwrap_err().is_conflict());
        assert!(duplicate_name.unwrap_err().is_conflict());
        other_workspace.unwrap();

        let mut tx = begin(&pool).await.unwrap();
        archive_credential(&mut tx, one, first.id).await.unwrap();
        commit(tx).await.unwrap();
        let reused = secret(&pool, &keys, one, SecretPurpose::WorkspaceCredential, b"{}").await;
        create_credential(&pool, one, &new("mail", "Mail", reused, &fields, &hints))
            .await
            .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_secret_of_another_workspace_cannot_be_linked(pool: PgPool) {
        let (one, two) = (workspace(&pool, "one").await, workspace(&pool, "two").await);
        let keys = Keyring::new(MasterKey::generate(), None).unwrap();
        let foreign = secret(&pool, &keys, two, SecretPurpose::WorkspaceCredential, b"{}").await;
        let (fields, hints) = (json!({}), json!({}));

        let linked =
            create_credential(&pool, one, &new("mail", "Mail", foreign, &fields, &hints)).await;

        assert!(linked.is_err());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn rotation_swaps_the_secret_and_the_old_row_is_deleted_in_one_transaction(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let keys = Keyring::new(MasterKey::generate(), None).unwrap();
        let before = credential(&pool, ws, "mail").await;
        let replacement = secret(&pool, &keys, ws, SecretPurpose::WorkspaceCredential, b"{}").await;

        let mut tx = begin(&pool).await.unwrap();
        let old = swap_credential_secret(
            &mut tx,
            ws,
            before.id,
            replacement,
            &json!({"api_key": "1234"}),
            Some("ff"),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(secrets::delete(&mut *tx, ws, old).await.unwrap());
        commit(tx).await.unwrap();

        let after = find_credential(&pool, ws, before.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(old.as_uuid(), before.secret_id);
        assert_eq!(after.secret_id, replacement.as_uuid());
        assert_eq!(after.hints, json!({"api_key": "1234"}));
        assert_eq!(after.fingerprint.as_deref(), Some("ff"));
        assert!(after.rotated_at >= before.rotated_at);
        assert!(secrets::get(&pool, ws, old).await.unwrap().is_none());
        assert!(
            secrets::get(&pool, ws, replacement)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_rotation_that_rolls_back_leaves_the_old_secret_in_place(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let keys = Keyring::new(MasterKey::generate(), None).unwrap();
        let before = credential(&pool, ws, "mail").await;
        let replacement = secret(&pool, &keys, ws, SecretPurpose::WorkspaceCredential, b"{}").await;

        let mut tx = begin(&pool).await.unwrap();
        let old =
            swap_credential_secret(&mut tx, ws, before.id, replacement, &json!({}), None, None)
                .await
                .unwrap()
                .unwrap();
        secrets::delete(&mut *tx, ws, old).await.unwrap();
        drop(tx);

        let after = find_credential(&pool, ws, before.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.secret_id, before.secret_id);
        assert!(
            secrets::get(&pool, ws, SecretId::from_uuid(before.secret_id))
                .await
                .unwrap()
                .is_some()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_old_secret_cannot_be_deleted_while_the_credential_points_at_it(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let before = credential(&pool, ws, "mail").await;

        let deleted = secrets::delete(&pool, ws, SecretId::from_uuid(before.secret_id)).await;

        assert!(deleted.is_err());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn archiving_with_a_live_reference_is_refused_and_a_released_credential_archives(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let shared = credential(&pool, ws, "mail").await;
        let config = json!({});
        let source = create_source(
            &pool,
            ws,
            &NewSource {
                id: Uuid::now_v7(),
                kind: "fake",
                name: "S",
                config: &config,
                credential_id: Some(shared.id),
                schedule: None,
                created_by: None,
            },
        )
        .await
        .unwrap();

        let mut tx = begin(&pool).await.unwrap();
        let refused = archive_credential(&mut tx, ws, shared.id).await.unwrap();
        drop(tx);

        let ArchiveOutcome::InUse(users) = refused else {
            panic!("expected InUse, got {refused:?}");
        };
        assert_eq!(users.len(), 1);
        assert_eq!((users[0].kind.as_str(), users[0].id), ("source", source.id));
        assert!(
            find_credential(&pool, ws, shared.id)
                .await
                .unwrap()
                .unwrap()
                .archived_at
                .is_none()
        );

        let mut tx = begin(&pool).await.unwrap();
        crate::delete_source(&mut tx, ws, source.id).await.unwrap();
        let outcome = archive_credential(&mut tx, ws, shared.id).await.unwrap();
        commit(tx).await.unwrap();
        assert!(matches!(outcome, ArchiveOutcome::Archived(_)));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_held_use_lock_blocks_an_archive_which_then_sees_the_new_reference(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let shared = credential(&pool, ws, "mail").await;
        let config = json!({});

        let mut linking = begin(&pool).await.unwrap();
        lock_credential_for_use(&mut linking, ws, shared.id)
            .await
            .unwrap()
            .unwrap();
        let archiver_pool = pool.clone();
        let id = shared.id;
        let archiver = tokio::spawn(async move {
            let mut tx = begin(&archiver_pool).await.unwrap();
            archive_credential(&mut tx, ws, id).await.unwrap()
        });
        wait_until_a_backend_waits_on_a_lock(&pool).await;
        assert!(
            !archiver.is_finished(),
            "the archive waits for the linking transaction"
        );

        create_source(
            &mut *linking,
            ws,
            &NewSource {
                id: Uuid::now_v7(),
                kind: "fake",
                name: "S",
                config: &config,
                credential_id: Some(shared.id),
                schedule: None,
                created_by: None,
            },
        )
        .await
        .unwrap();
        commit(linking).await.unwrap();

        let ArchiveOutcome::InUse(users) = archiver.await.unwrap() else {
            panic!("the archive must see the reference the linking transaction committed");
        };
        assert_eq!(users.len(), 1);
        let kept = find_credential(&pool, ws, shared.id)
            .await
            .unwrap()
            .unwrap();
        assert!(kept.archived_at.is_none());
    }

    /// One rotation as the API makes it: lock the credential, seal and insert a new secret, swap
    /// it in and delete the old one. `holding` tells the test the lock is taken and then waits for
    /// `release` before swapping, so a second rotation provably queues behind it.
    async fn rotate_once(
        pool: PgPool,
        keys: std::sync::Arc<Keyring>,
        ws: WorkspaceId,
        id: Uuid,
        round: u8,
        gate: Option<(
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    ) {
        let mut tx = begin(&pool).await.unwrap();
        find_credential_for_update(&mut tx, ws, id)
            .await
            .unwrap()
            .unwrap();
        if let Some((holding, release)) = gate {
            holding.send(()).unwrap();
            release.await.unwrap();
        }
        let new_id = SecretId::new();
        let purpose = SecretPurpose::WorkspaceCredential;
        let plaintext = format!("{{\"api_key\":\"round-{round}\"}}");
        let sealed = seal(
            &keys,
            plaintext.as_bytes(),
            &secret_aad(ws, new_id, purpose),
        );
        secrets::insert_unchecked(&mut *tx, ws, purpose, new_id, &sealed)
            .await
            .unwrap();
        let old =
            swap_credential_secret(&mut tx, ws, id, new_id, &json!({"api_key": ""}), None, None)
                .await
                .unwrap()
                .unwrap();
        assert!(secrets::delete(&mut *tx, ws, old).await.unwrap());
        commit(tx).await.unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn two_concurrent_rotations_leave_one_secret_row_and_a_credential_that_opens(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let keys = std::sync::Arc::new(Keyring::new(MasterKey::generate(), None).unwrap());
        let before = credential(&pool, ws, "mail").await;
        let (holding_tx, holding_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();

        let first = tokio::spawn(rotate_once(
            pool.clone(),
            keys.clone(),
            ws,
            before.id,
            0,
            Some((holding_tx, release_rx)),
        ));
        holding_rx.await.unwrap();
        let second = tokio::spawn(rotate_once(
            pool.clone(),
            keys.clone(),
            ws,
            before.id,
            1,
            None,
        ));
        wait_until_a_backend_waits_on_a_lock(&pool).await;
        assert!(
            !second.is_finished(),
            "the second rotation waits for the first"
        );
        release_tx.send(()).unwrap();
        first.await.unwrap();
        second.await.unwrap();

        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM encrypted_secrets")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1, "each rotation deleted the secret it replaced");
        let opened = crate::open_credential(&pool, &keys, ws, before.id)
            .await
            .unwrap();
        let text = String::from_utf8(opened.secret.expose().to_vec()).unwrap();
        assert!(
            text.contains("round-1"),
            "the second rotation won last: {text}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_archived_credential_cannot_be_locked_for_use_and_another_workspaces_is_not_found(
        pool: PgPool,
    ) {
        let (one, two) = (workspace(&pool, "one").await, workspace(&pool, "two").await);
        let live = credential(&pool, one, "live").await;
        let gone = credential(&pool, one, "gone").await;
        let mut tx = begin(&pool).await.unwrap();
        archive_credential(&mut tx, one, gone.id).await.unwrap();

        assert!(
            lock_credential_for_use(&mut tx, one, live.id)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            lock_credential_for_use(&mut tx, one, gone.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            lock_credential_for_use(&mut tx, two, live.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            find_credential(&mut *tx, two, live.id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_index_of_the_credentials_table_leads_with_the_workspace(pool: PgPool) {
        let leading: Vec<(String, String)> = sqlx::query_as(
            "SELECT i.indexrelid::regclass::text, a.attname::text \
             FROM pg_index i \
             JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = i.indkey[0] \
             WHERE i.indrelid = 'credentials'::regclass AND NOT i.indisprimary",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        assert!(leading.len() >= 4, "{leading:?}");
        for (index, column) in &leading {
            assert_eq!(column, "workspace_id", "{index} leads with {column}");
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_credentials_table_holds_no_person_or_address_column_for_the_erasure_scan(
        pool: PgPool,
    ) {
        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT table_name::text || '.' || column_name::text \
             FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = 'credentials' \
               AND (column_name = 'person_id' OR column_name LIKE '%\\_person\\_id' \
                    OR column_name ~ '(email|address|phone|recipient|\\mto\\M)') \
             ORDER BY 1",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let uuid_person: Vec<String> = sqlx::query_scalar(
            "SELECT table_name::text FROM information_schema.columns \
             WHERE table_schema = current_schema() AND data_type = 'uuid' \
               AND (column_name = 'person_id' OR column_name LIKE '%\\_person\\_id') \
               AND table_name = 'credentials'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        assert!(columns.is_empty(), "{columns:?}");
        assert!(uuid_person.is_empty(), "{uuid_person:?}");
    }
}
