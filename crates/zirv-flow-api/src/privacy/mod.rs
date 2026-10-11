//! Deletion, erasure and export of a person (architecture sections 7.4 and 16.3). Every
//! endpoint needs the workspace admin role: an export carries values, and an erasure cannot be
//! undone. Nothing here audits, logs or labels an identifier, a trait or an event value.

pub(crate) mod erasures;
pub(crate) mod exports;
pub(crate) mod person;
pub(crate) mod settings;

use serde::Deserialize;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::people::Identifier;
use zirv_flow_core::{ValidationError, WorkspaceId};
use zirv_flow_store::PgPool;

use crate::admin::AdminError;
use crate::auth::Principal;

#[derive(Deserialize, ToSchema)]
pub(crate) struct TargetIdentifier {
    /// `email`, `phone`, `anonymous_id` or `external:<namespace>`.
    #[schema(example = "email")]
    kind: String,
    /// The identifier value. It is looked up and not stored.
    value: String,
}

/// The person a privacy request is about: give a `person_id` or an `identifier`, not both.
#[derive(Deserialize, ToSchema)]
pub(crate) struct PersonTarget {
    /// The person, or the person they were merged into.
    person_id: Option<Uuid>,
    /// An identifier of the person; it resolves to their current person.
    identifier: Option<TargetIdentifier>,
}

/// The person the request names, after merges. A person or identifier the workspace does not
/// know is `NotFound`; a body that names both or neither is invalid.
pub(crate) async fn resolve(
    pool: &PgPool,
    workspace: WorkspaceId,
    target: &PersonTarget,
) -> Result<Uuid, AdminError> {
    match (&target.person_id, &target.identifier) {
        (Some(id), None) => zirv_flow_store::current_person(pool, workspace, *id)
            .await?
            .ok_or(AdminError::NotFound("person")),
        (None, Some(identifier)) => {
            let identifier = Identifier::parse(&identifier.kind, &identifier.value)
                .map_err(|error| ValidationError::new("identifier", error.reason()))?;
            zirv_flow_store::find_person_by_identifier(pool, workspace, &identifier)
                .await?
                .map(|person| person.id)
                .ok_or(AdminError::NotFound("person"))
        }
        _ => Err(ValidationError::new("body", "must give either person_id or identifier").into()),
    }
}

/// The signed-in user behind a request; none for an API key.
fn requested_by(principal: &Principal) -> Option<Uuid> {
    principal.user_id().map(|id| id.as_uuid())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(json: &str) -> PersonTarget {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn a_target_is_a_person_id_or_an_identifier() {
        let by_id = target(r#"{"person_id": "00000000-0000-7000-8000-000000000000"}"#);
        let by_identifier = target(r#"{"identifier": {"kind": "email", "value": "a@b.c"}}"#);

        assert!(by_id.person_id.is_some() && by_id.identifier.is_none());
        assert!(by_identifier.person_id.is_none() && by_identifier.identifier.is_some());
        assert!(serde_json::from_str::<PersonTarget>(r#"{"person_id": "nope"}"#).is_err());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_target_that_names_both_or_neither_is_invalid_and_an_unknown_one_is_not_found(
        pool: sqlx::PgPool,
    ) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let name = zirv_flow_core::WorkspaceName::parse("one").unwrap();
        let ws = zirv_flow_store::create_workspace(
            &pool,
            &name,
            &zirv_flow_core::Slug::parse("one").unwrap(),
        )
        .await
        .unwrap()
        .id;
        let both = target(
            r#"{"person_id": "00000000-0000-7000-8000-000000000000",
                "identifier": {"kind": "email", "value": "a@b.c"}}"#,
        );
        let neither = target("{}");
        let unknown_id = target(r#"{"person_id": "00000000-0000-7000-8000-000000000000"}"#);
        let unknown = target(r#"{"identifier": {"kind": "email", "value": "a@b.c"}}"#);
        let malformed = target(r#"{"identifier": {"kind": "email", "value": "not an email"}}"#);

        assert!(matches!(
            resolve(&pool, ws, &both).await,
            Err(AdminError::Invalid(_))
        ));
        assert!(matches!(
            resolve(&pool, ws, &neither).await,
            Err(AdminError::Invalid(_))
        ));
        assert!(matches!(
            resolve(&pool, ws, &unknown_id).await,
            Err(AdminError::NotFound(_))
        ));
        assert!(matches!(
            resolve(&pool, ws, &unknown).await,
            Err(AdminError::NotFound(_))
        ));
        assert!(matches!(
            resolve(&pool, ws, &malformed).await,
            Err(AdminError::Invalid(_))
        ));
    }
}
