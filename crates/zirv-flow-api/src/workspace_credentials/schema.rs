//! The shape of a workspace credential on the wire: the schema of a kind, the list of kinds, and
//! the schema a stored credential resolves to. A kind lists fields with their types and which
//! are secret, never a value, so a form can be drawn from it.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;
use zirv_flow_core::credentials::{
    CredentialField, CredentialKind, CredentialSchema, FieldType, IMPORTED_KIND, Issue,
};
use zirv_flow_store::WorkspaceCredential;

use crate::AppState;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::problem::{ApiError, Problem};
use crate::sources::problems;

/// The value type of a credential field.
#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FieldTypeBody {
    String,
    Integer,
    Enum,
    Url,
}

impl From<FieldType> for FieldTypeBody {
    fn from(field_type: FieldType) -> Self {
        match field_type {
            FieldType::String => Self::String,
            FieldType::Integer => Self::Integer,
            FieldType::Enum => Self::Enum,
            FieldType::Url => Self::Url,
        }
    }
}

impl From<FieldTypeBody> for FieldType {
    fn from(field_type: FieldTypeBody) -> Self {
        match field_type {
            FieldTypeBody::String => Self::String,
            FieldTypeBody::Integer => Self::Integer,
            FieldTypeBody::Enum => Self::Enum,
            FieldTypeBody::Url => Self::Url,
        }
    }
}

/// One field of a credential schema.
#[derive(Clone, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SchemaFieldBody {
    /// Lowercase letters, digits and underscores, starting with a letter.
    #[schema(example = "api_key")]
    key: String,
    #[schema(example = "API key")]
    label: String,
    #[serde(rename = "type")]
    field_type: FieldTypeBody,
    #[serde(default)]
    required: bool,
    /// A secret field is sealed and never returned. Only a `string` can be secret.
    #[serde(default)]
    secret: bool,
    /// The allowed values of an `enum` field.
    #[serde(default)]
    options: Vec<String>,
}

/// A list of fields: the shape of one credential.
#[derive(Clone, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SchemaBody {
    fields: Vec<SchemaFieldBody>,
}

impl From<&CredentialSchema> for SchemaBody {
    fn from(schema: &CredentialSchema) -> Self {
        Self {
            fields: schema
                .fields
                .iter()
                .map(|field| SchemaFieldBody {
                    key: field.key.clone(),
                    label: field.label.clone(),
                    field_type: field.field_type.into(),
                    required: field.required,
                    secret: field.secret,
                    options: field.options.clone(),
                })
                .collect(),
        }
    }
}

impl From<&SchemaBody> for CredentialSchema {
    fn from(body: &SchemaBody) -> Self {
        Self {
            fields: body
                .fields
                .iter()
                .map(|field| CredentialField {
                    key: field.key.clone(),
                    label: field.label.clone(),
                    field_type: field.field_type.into(),
                    required: field.required,
                    secret: field.secret,
                    options: field.options.clone(),
                })
                .collect(),
        }
    }
}

/// A 422 that names each issue under `group`, never a value.
pub(crate) fn issue_problems(group: &str, issues: Vec<Issue>) -> ApiError {
    problems(
        issues
            .into_iter()
            .map(|issue| {
                (
                    format!("{group}.{}", issue.field),
                    issue.code.message().to_owned(),
                )
            })
            .collect(),
    )
}

/// The schema a stored credential has: its built-in kind's, its own `custom` schema, or, for an
/// `imported` credential, one string secret per field it holds.
pub(crate) fn resolved_schema(
    credential: &WorkspaceCredential,
) -> Result<CredentialSchema, ApiError> {
    if let Some(kind) = CredentialKind::parse(&credential.kind) {
        return Ok(kind.schema());
    }
    if credential.kind == IMPORTED_KIND {
        let names = credential
            .hints
            .as_object()
            .map(|hints| hints.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        return Ok(CredentialSchema {
            fields: names
                .into_iter()
                .map(|key| CredentialField {
                    label: key.clone(),
                    key,
                    field_type: FieldType::String,
                    required: true,
                    secret: true,
                    options: Vec::new(),
                })
                .collect(),
        });
    }
    credential
        .schema
        .clone()
        .and_then(|schema: Value| serde_json::from_value(schema).ok())
        .ok_or_else(ApiError::internal)
}

/// One credential kind with its schema.
#[derive(Serialize, ToSchema)]
pub(crate) struct KindBody {
    /// The id stored in `credential.kind`.
    #[schema(example = "api_key")]
    id: String,
    #[schema(example = "API key")]
    label: String,
    schema: SchemaBody,
}

/// The built-in kinds, then `custom` (the schema comes with the credential).
pub(crate) fn kinds() -> Vec<KindBody> {
    CredentialKind::ALL
        .into_iter()
        .map(|kind| KindBody {
            id: kind.as_str().to_owned(),
            label: kind.label().to_owned(),
            schema: SchemaBody::from(&kind.schema()),
        })
        .collect()
}

/// The credential kinds and their schemas, so a form can be drawn from data. A kind named
/// `custom` takes its schema in the create request. Never holds a value. Workspace admins only.
#[utoipa::path(
    get,
    operation_id = "list_credential_kinds",
    path = "/api/v1/workspaces/{ws}/credential-kinds",
    tag = "credentials",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The built-in kinds with their schemas", body = Vec<KindBody>),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or a missing scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn list_kinds(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Json<Vec<KindBody>>, ApiError> {
    authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    Ok(Json(kinds()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;
    use zirv_flow_core::credentials::IssueCode;

    use super::*;

    fn stored(kind: &str, schema: Option<Value>, hints: Value) -> WorkspaceCredential {
        WorkspaceCredential {
            id: Uuid::nil(),
            key: "k".into(),
            name: "n".into(),
            kind: kind.into(),
            schema_version: 1,
            schema,
            fields: json!({}),
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
    fn the_kinds_list_every_built_in_with_its_fields_and_no_value() {
        let listed = serde_json::to_value(kinds()).unwrap();

        let ids: Vec<_> = listed
            .as_array()
            .unwrap()
            .iter()
            .map(|kind| kind["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            [
                "api_key",
                "basic",
                "smtp",
                "oauth2_client",
                "signing_secret"
            ]
        );
        let smtp = &listed[2]["schema"]["fields"];
        assert_eq!(smtp[4]["key"], "password");
        assert_eq!(smtp[4]["secret"], true);
        assert_eq!(smtp[2]["options"], json!(["starttls", "tls", "none"]));
        assert!(!listed.to_string().contains("\"value\""));
    }

    #[test]
    fn a_schema_body_round_trips_and_refuses_unknown_keys() {
        let schema = CredentialKind::Smtp.schema();
        let body = SchemaBody::from(&schema);

        let wire = serde_json::to_value(&body).unwrap();
        let back: SchemaBody = serde_json::from_value(wire).unwrap();

        assert_eq!(CredentialSchema::from(&back), schema);
        let extra = json!({"fields": [], "extra": 1});
        assert!(serde_json::from_value::<SchemaBody>(extra).is_err());
        let field = json!({"fields": [{"key": "a", "label": "A", "type": "string", "x": 1}]});
        assert!(serde_json::from_value::<SchemaBody>(field).is_err());
    }

    #[test]
    fn a_stored_credential_resolves_to_its_kind_its_own_schema_or_one_secret_per_field() {
        let builtin = resolved_schema(&stored("basic", None, json!({}))).unwrap();
        assert_eq!(builtin, CredentialKind::Basic.schema());

        let own = CredentialSchema {
            fields: vec![CredentialField {
                key: "token".into(),
                label: "Token".into(),
                field_type: FieldType::String,
                required: true,
                secret: true,
                options: Vec::new(),
            }],
        };
        let custom = resolved_schema(&stored(
            "custom",
            Some(serde_json::to_value(&own).unwrap()),
            json!({}),
        ))
        .unwrap();
        assert_eq!(custom, own);

        let imported = resolved_schema(&stored(
            IMPORTED_KIND,
            None,
            json!({"user": "", "password": ""}),
        ))
        .unwrap();
        let keys: Vec<_> = imported.secret_keys().collect();
        assert_eq!(keys, ["password", "user"]);
        assert!(imported.fields.iter().all(|field| field.required));

        assert!(resolved_schema(&stored("custom", None, json!({}))).is_err());
    }

    #[test]
    fn issue_problems_name_the_group_and_the_field_and_no_value() {
        let error = issue_problems(
            "secret",
            vec![Issue {
                field: "token".into(),
                code: IssueCode::BadLength,
            }],
        );

        assert_eq!(error.detail(), "secret.token must be 1 to 8192 bytes");
    }
}
