//! Opening the sealed secret of a workspace credential, right before use.

use std::fmt;

use serde_json::Value;
use sqlx::PgExecutor;
use sqlx::prelude::FromRow;
use uuid::Uuid;
use zirv_flow_core::{Keyring, Sealed, SecretBytes, SecretId, SecretPurpose, WorkspaceId};

use crate::StoreError;
use crate::secrets::OpenError;

/// What a credential holds once opened: its non-secret values and the plaintext of its sealed
/// secret (a JSON object of every secret field), wiped on drop. No `Serialize`, no `Clone`, and
/// `Debug` shows nothing.
pub struct OpenedCredential {
    /// The non-secret values.
    pub fields: Value,
    /// The plaintext of the sealed secret.
    pub secret: SecretBytes,
}

impl fmt::Debug for OpenedCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OpenedCredential([redacted])")
    }
}

#[derive(FromRow)]
struct OpenRow {
    fields: Value,
    secret_id: Uuid,
    purpose: String,
    key_id: String,
    wrapped_key: Vec<u8>,
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
}

/// Opens the credential's current secret. The credential row and the secret row are read in one
/// statement, so a rotation that commits in between cannot leave this call with a deleted id: it
/// sees the old pair or the new pair. The secret opens under the purpose it was sealed for, which
/// the associated data binds: `workspace_credential` for a credential made here, and an older
/// `connector_credential` or `destination_credential` for one imported in place. Any other
/// purpose does not open.
pub async fn open_credential<'e>(
    executor: impl PgExecutor<'e>,
    keyring: &Keyring,
    workspace: WorkspaceId,
    credential: Uuid,
) -> Result<OpenedCredential, OpenError> {
    let row: Option<OpenRow> = sqlx::query_as(
        "SELECT c.fields, c.secret_id, s.purpose, s.key_id, s.wrapped_key, s.nonce, s.ciphertext \
         FROM credentials c \
         JOIN encrypted_secrets s ON s.workspace_id = c.workspace_id AND s.id = c.secret_id \
         WHERE c.workspace_id = $1 AND c.id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(credential)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    let row = row.ok_or(OpenError::NotFound)?;
    let purpose = SecretPurpose::parse(&row.purpose)
        .filter(|purpose| {
            matches!(
                purpose,
                SecretPurpose::WorkspaceCredential
                    | SecretPurpose::ConnectorCredential
                    | SecretPurpose::DestinationCredential
            )
        })
        .ok_or(OpenError::Unopenable)?;
    let nonce: [u8; 24] = row.nonce.try_into().map_err(|_| OpenError::Unopenable)?;
    let sealed = Sealed {
        key_id: row.key_id,
        wrapped_key: row.wrapped_key,
        nonce,
        ciphertext: row.ciphertext,
    };
    let aad = zirv_flow_core::secret_aad(workspace, SecretId::from_uuid(row.secret_id), purpose);
    let secret = zirv_flow_core::open(keyring, &sealed, &aad).map_err(|_| OpenError::Unopenable)?;
    Ok(OpenedCredential {
        fields: row.fields,
        secret,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use zirv_flow_core::MasterKey;

    use super::*;
    use crate::workspace_credentials::testing::{secret, workspace};
    use crate::workspace_credentials::{NewWorkspaceCredential, create_credential};

    async fn credential_with(
        pool: &PgPool,
        keys: &Keyring,
        ws: WorkspaceId,
        purpose: SecretPurpose,
        key: &str,
    ) -> Uuid {
        let secret_id = secret(pool, keys, ws, purpose, br#"{"password":"hunter2"}"#).await;
        create_credential(
            pool,
            ws,
            &NewWorkspaceCredential {
                id: Uuid::now_v7(),
                key,
                name: key,
                kind: "basic",
                schema: None,
                fields: &json!({"username": "u"}),
                secret_id,
                hints: &json!({"password": ""}),
                fingerprint: None,
                created_by: None,
            },
        )
        .await
        .unwrap()
        .id
    }

    #[test]
    fn debug_shows_nothing() {
        let opened = OpenedCredential {
            fields: json!({"username": "u"}),
            secret: SecretBytes::new(b"hunter2".to_vec()),
        };

        let debug = format!("{opened:?}");

        assert!(
            !debug.contains("hunter2") && !debug.contains("username"),
            "{debug}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_credential_opens_under_the_purpose_it_was_sealed_for(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let keys = Keyring::new(MasterKey::generate(), None).unwrap();
        for (purpose, key) in [
            (SecretPurpose::WorkspaceCredential, "new"),
            (SecretPurpose::ConnectorCredential, "source"),
            (SecretPurpose::DestinationCredential, "destination"),
        ] {
            let id = credential_with(&pool, &keys, ws, purpose, key).await;

            let opened = open_credential(&pool, &keys, ws, id).await.unwrap();

            assert_eq!(opened.fields, json!({"username": "u"}), "{key}");
            assert_eq!(
                opened.secret.expose(),
                br#"{"password":"hunter2"}"#,
                "{key}"
            );
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn another_workspace_a_wrong_key_and_a_foreign_purpose_do_not_open(pool: PgPool) {
        let (one, two) = (workspace(&pool, "one").await, workspace(&pool, "two").await);
        let keys = Keyring::new(MasterKey::generate(), None).unwrap();
        let id = credential_with(&pool, &keys, one, SecretPurpose::WorkspaceCredential, "a").await;
        let ai = credential_with(&pool, &keys, one, SecretPurpose::AiProviderKey, "ai").await;
        let stranger = Keyring::new(MasterKey::generate(), None).unwrap();

        assert!(matches!(
            open_credential(&pool, &keys, two, id).await,
            Err(OpenError::NotFound)
        ));
        assert!(matches!(
            open_credential(&pool, &stranger, one, id).await,
            Err(OpenError::Unopenable)
        ));
        assert!(matches!(
            open_credential(&pool, &keys, one, ai).await,
            Err(OpenError::Unopenable)
        ));
        assert!(matches!(
            open_credential(&pool, &keys, one, Uuid::now_v7()).await,
            Err(OpenError::NotFound)
        ));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_master_key_rotation_rewraps_the_credential_secret_and_it_opens_with_the_new_key_only(
        pool: PgPool,
    ) {
        use zirv_flow_core::{deployment_key_aad, rewrap};

        let ws = workspace(&pool, "one").await;
        let old = Keyring::new(MasterKey::generate(), None).unwrap();
        let id = credential_with(&pool, &old, ws, SecretPurpose::WorkspaceCredential, "a").await;
        let root = zirv_flow_core::seal(
            &old,
            b"root",
            &deployment_key_aad(crate::secrets::HMAC_ROOT),
        );
        crate::secrets::insert_hmac_root_if_absent(&pool, &root)
            .await
            .unwrap();
        let rotated = Keyring::new(
            MasterKey::generate(),
            Some(MasterKey::parse(&old.current().to_base64()).unwrap()),
        )
        .unwrap();
        let before = crate::find_credential(&pool, ws, id)
            .await
            .unwrap()
            .unwrap();

        let mut tx = crate::begin(&pool).await.unwrap();
        let report = crate::secrets::rotate(&mut tx, &rotated.current().key_id(), |sealed| {
            rewrap(&rotated, sealed)
        })
        .await
        .unwrap();
        crate::commit(tx).await.unwrap();

        assert_eq!(report.rows, 2);
        let new_only = Keyring::new(
            MasterKey::parse(&rotated.current().to_base64()).unwrap(),
            None,
        )
        .unwrap();
        let opened = open_credential(&pool, &new_only, ws, id).await.unwrap();
        assert_eq!(opened.secret.expose(), br#"{"password":"hunter2"}"#);
        assert!(matches!(
            open_credential(&pool, &old, ws, id).await,
            Err(OpenError::Unopenable)
        ));
        let after = crate::find_credential(&pool, ws, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.secret_id, before.secret_id);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_secret_moved_to_another_purpose_in_the_row_no_longer_opens(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let keys = Keyring::new(MasterKey::generate(), None).unwrap();
        let id = credential_with(&pool, &keys, ws, SecretPurpose::ConnectorCredential, "a").await;

        sqlx::query("UPDATE encrypted_secrets SET purpose = 'destination_credential'")
            .execute(&pool)
            .await
            .unwrap();

        assert!(matches!(
            open_credential(&pool, &keys, ws, id).await,
            Err(OpenError::Unopenable)
        ));
    }
}
