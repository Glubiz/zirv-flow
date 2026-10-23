//! How a source or destination reaches its workspace credential: opening it right before a call,
//! and linking it when the source or destination is saved.

use std::collections::BTreeSet;

use uuid::Uuid;
use zirv_flow_connectors::sources::{Credential, FieldError};
use zirv_flow_core::WorkspaceId;
use zirv_flow_store::{PgConnection, WorkspaceCredential};

use super::archived;
use crate::AppState;
use crate::problem::ApiError;
use crate::sources::credential::keyring;
use crate::sources::{field_problems, invalid};

/// Opens the workspace credential a source or destination uses: the non-secret fields and the
/// current sealed secret, merged into the typed, zeroizing credential a connector takes. An
/// owner with none gets an empty credential. The secret is read when this runs, so a rotation
/// reaches every owner on its next call.
pub(crate) async fn load(
    state: &AppState,
    workspace: WorkspaceId,
    credential: Option<Uuid>,
) -> Result<Credential, ApiError> {
    let Some(credential) = credential else {
        return Ok(Credential::new());
    };
    let opened =
        zirv_flow_store::open_credential(&state.pool, keyring(state)?, workspace, credential)
            .await
            .map_err(|error| {
                tracing::warn!(?error, "a workspace credential could not be opened");
                ApiError::internal()
            })?;
    Credential::from_parts(&opened.secret, &opened.fields).map_err(|_| ApiError::internal())
}

/// The names of the fields a credential holds: its non-secret fields and its secret fields.
pub(crate) fn field_names(credential: &WorkspaceCredential) -> BTreeSet<String> {
    let keys = |value: &serde_json::Value| -> Vec<String> {
        value
            .as_object()
            .map(|object| object.keys().cloned().collect())
            .unwrap_or_default()
    };
    keys(&credential.fields)
        .into_iter()
        .chain(keys(&credential.hints))
        .collect()
}

/// Checks the credential an `http` source uses against the credential schema its saved manifest
/// declares in `auth.credential`, if it declares one: every required field must be present, a
/// secret field among the credential's secrets and a plain one among its `fields`. With no
/// credential the check runs against nothing, so a manifest that requires a secret refuses a
/// source that has none. A manifest that declares no schema, or is not valid (the config check
/// reports that), asks nothing more.
pub(crate) fn check_manifest_credential(
    config: &serde_json::Value,
    credential: Option<&WorkspaceCredential>,
) -> Result<(), ApiError> {
    let Some(manifest) = config.get("manifest") else {
        return Ok(());
    };
    let Some(spec) = zirv_flow_connectors::manifest::validate(manifest)
        .ok()
        .and_then(|manifest| manifest.credential)
    else {
        return Ok(());
    };
    let keys = |value: Option<&serde_json::Value>| -> BTreeSet<String> {
        value
            .and_then(serde_json::Value::as_object)
            .map(|object| object.keys().cloned().collect())
            .unwrap_or_default()
    };
    let plain = keys(credential.map(|stored| &stored.fields));
    let sealed = keys(credential.map(|stored| &stored.hints));
    let mut errors = Vec::new();
    for field in spec.schema().fields {
        let (here, there) = if field.secret {
            (&sealed, &plain)
        } else {
            (&plain, &sealed)
        };
        let reason = if there.contains(&field.key) {
            Some(
                "is in the wrong group for this manifest: it is secret in one place and plain in the other",
            )
        } else if field.required && !here.contains(&field.key) {
            Some("is required by the manifest and the credential has no such field")
        } else {
            None
        };
        if let Some(reason) = reason {
            errors.push(FieldError {
                field: field.key,
                reason: reason.to_owned(),
            });
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(field_problems("credential", errors))
    }
}

/// Checks that a credential as it will be after a change (`after`) still holds every field the
/// live sources and destinations that use it require, with the same field problems the link
/// check gives. The caller holds the credential's row lock, and linking takes a share lock on it,
/// so no owner appears between this check and the commit.
pub(crate) async fn check_owners(
    tx: &mut PgConnection,
    workspace: WorkspaceId,
    after: &WorkspaceCredential,
) -> Result<(), ApiError> {
    let present = field_names(after);
    for user in zirv_flow_store::credential_users(&mut *tx, workspace, after.id).await? {
        let errors = match user.kind.as_str() {
            "source" => {
                let Some(source) =
                    zirv_flow_store::find_source(&mut *tx, workspace, user.id).await?
                else {
                    continue;
                };
                let Some(driver) = crate::sources::registry().get(&source.kind) else {
                    continue;
                };
                if source.kind == "http" {
                    check_manifest_credential(&source.config, Some(after))?;
                }
                zirv_flow_connectors::sources::validate_credential_names(driver.spec(), &present)
            }
            _ => {
                let Some(destination) =
                    zirv_flow_store::find_destination(&mut *tx, workspace, user.id).await?
                else {
                    continue;
                };
                let registry = crate::destinations::registry();
                let Some(driver) = registry.get(&destination.kind) else {
                    continue;
                };
                zirv_flow_connectors::destinations::validate_credential_names(
                    driver.spec(),
                    &present,
                )
            }
        };
        errors.map_err(|errors| field_problems("credential", errors))?;
    }
    Ok(())
}

/// Locks a live credential of the workspace `FOR SHARE` for the rest of the transaction and checks
/// that it holds every credential field the owner requires (`check` is the owner type's rule).
/// An archive waits for the owner's transaction, so an archived credential is never referenced.
/// A credential of another workspace is a 422, as an unknown id is.
pub(crate) async fn lock_for_use(
    tx: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    check: impl FnOnce(&BTreeSet<String>) -> Result<(), Vec<FieldError>>,
) -> Result<WorkspaceCredential, ApiError> {
    match zirv_flow_store::lock_credential_for_use(tx, workspace, id).await? {
        Some(credential) => {
            check(&field_names(&credential))
                .map_err(|errors| field_problems("credential", errors))?;
            Ok(credential)
        }
        None => match zirv_flow_store::find_credential(&mut *tx, workspace, id).await? {
            Some(_) => Err(archived()),
            None => Err(invalid(
                "credential_id",
                "is not a credential of this workspace",
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;

    use super::*;

    fn stored(fields: serde_json::Value, hints: serde_json::Value) -> WorkspaceCredential {
        WorkspaceCredential {
            id: Uuid::nil(),
            key: "k".into(),
            name: "n".into(),
            kind: "custom".into(),
            schema_version: 1,
            schema: None,
            fields,
            secret_id: Uuid::nil(),
            hints,
            fingerprint: None,
            rotated_at: time::OffsetDateTime::UNIX_EPOCH,
            rotated_by: None,
            last_tested_at: None,
            last_test_outcome: None,
            archived_at: None,
            created_by: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn the_field_names_are_the_non_secret_fields_and_the_secret_fields() {
        let credential = stored(json!({"host": "h"}), json!({"password": "abcd"}));

        let names: Vec<_> = field_names(&credential).into_iter().collect();

        assert_eq!(names, ["host", "password"]);
        assert!(field_names(&stored(json!({}), json!({}))).is_empty());
    }

    #[tokio::test]
    async fn an_owner_without_a_credential_has_an_empty_one() {
        let pool = sqlx::PgPool::connect_lazy("postgres://localhost/none").unwrap();
        let state = AppState::new(pool, std::sync::Arc::new(String::new));

        let credential = load(&state, WorkspaceId::new(), None).await.unwrap();

        assert!(credential.is_empty());
    }

    #[tokio::test]
    async fn a_credential_without_the_master_key_is_unavailable() {
        let pool = sqlx::PgPool::connect_lazy("postgres://localhost/none").unwrap();
        let state = AppState::new(pool, std::sync::Arc::new(String::new));

        let error = load(&state, WorkspaceId::new(), Some(Uuid::nil()))
            .await
            .err()
            .unwrap();

        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
