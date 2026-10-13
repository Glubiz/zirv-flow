//! The source types this build ships, with the fields the setup form renders.

use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;
use zirv_flow_connectors::sources::{FieldKind, FieldSpec, SourceSpec};

use super::registry;
use crate::auth::Principal;
use crate::problem::{ApiError, Problem};

#[derive(Serialize, ToSchema)]
pub(crate) struct FieldSpecBody {
    #[schema(example = "token")]
    key: String,
    label: String,
    help: String,
    /// One of `text`, `url`, `number`, `bool`, `select`, `hosts`, `textarea`.
    kind: String,
    /// The allowed values of a `select` field; empty for the other kinds.
    options: Vec<String>,
    required: bool,
    /// True for a credential field: render a password input and never prefill it.
    secret: bool,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct CapabilitiesBody {
    incremental: bool,
    full_refresh: bool,
    emits_deletes: bool,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SourceKindBody {
    /// The stable id to send as `kind` when creating a source.
    #[schema(example = "fake")]
    kind: String,
    label: String,
    description: String,
    /// Plain settings, stored as the source's configuration.
    config_fields: Vec<FieldSpecBody>,
    /// Settings stored only in the encrypted credential.
    credential_fields: Vec<FieldSpecBody>,
    capabilities: CapabilitiesBody,
}

fn field(spec: &FieldSpec) -> FieldSpecBody {
    FieldSpecBody {
        key: spec.key.to_owned(),
        label: spec.label.to_owned(),
        help: spec.help.to_owned(),
        kind: spec.kind.name().to_owned(),
        options: match spec.kind {
            FieldKind::Select(options) => {
                options.iter().map(|option| (*option).to_owned()).collect()
            }
            _ => Vec::new(),
        },
        required: spec.required,
        secret: spec.secret,
    }
}

impl From<&SourceSpec> for SourceKindBody {
    fn from(spec: &SourceSpec) -> Self {
        Self {
            kind: spec.kind.to_owned(),
            label: spec.label.to_owned(),
            description: spec.description.to_owned(),
            config_fields: spec.config_fields.iter().map(field).collect(),
            credential_fields: spec.credential_fields.iter().map(field).collect(),
            capabilities: CapabilitiesBody {
                incremental: spec.capabilities.incremental,
                full_refresh: spec.capabilities.full_refresh,
                emits_deletes: spec.capabilities.emits_deletes,
            },
        }
    }
}

/// The source types this build ships, with their field specs. Any signed-in user or API key.
#[utoipa::path(
    get,
    operation_id = "list_source_kinds",
    path = "/api/v1/source-kinds",
    tag = "sources",
    responses(
        (status = 200, description = "The source types", body = Vec<SourceKindBody>),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn list(_principal: Principal) -> Result<Json<Vec<SourceKindBody>>, ApiError> {
    Ok(Json(
        registry()
            .specs()
            .into_iter()
            .map(SourceKindBody::from)
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use zirv_flow_connectors::sources::Registry;

    use super::*;

    #[test]
    fn the_fake_lists_its_fields_with_the_credential_marked_secret() {
        let registry = Registry::built_in();
        let body = SourceKindBody::from(registry.get("fake").unwrap().spec());

        let json = serde_json::to_value(&body).unwrap();

        assert_eq!(json["kind"], "fake");
        assert_eq!(json["config_fields"][0]["key"], "streams");
        assert_eq!(json["config_fields"][0]["kind"], "number");
        assert_eq!(json["config_fields"][0]["secret"], false);
        assert_eq!(json["credential_fields"][0]["key"], "token");
        assert_eq!(json["credential_fields"][0]["secret"], true);
        assert_eq!(json["credential_fields"][0]["required"], true);
        assert_eq!(json["capabilities"]["incremental"], true);
    }

    #[test]
    fn the_kafka_kind_lists_hosts_and_textarea_fields_and_only_secret_credentials() {
        let registry = Registry::built_in();
        let body = SourceKindBody::from(registry.get("kafka").unwrap().spec());

        let json = serde_json::to_value(&body).unwrap();

        let kind_of = |list: &str, key: &str| {
            json[list]
                .as_array()
                .unwrap()
                .iter()
                .find(|field| field["key"] == key)
                .map(|field| field["kind"].clone())
        };
        assert_eq!(kind_of("config_fields", "brokers"), Some("hosts".into()));
        assert_eq!(kind_of("config_fields", "tls_ca"), Some("textarea".into()));
        assert_eq!(kind_of("config_fields", "max_lag"), Some("number".into()));
        assert!(
            json["credential_fields"]
                .as_array()
                .unwrap()
                .iter()
                .all(|field| field["secret"] == true)
        );
        assert_eq!(json["capabilities"]["emits_deletes"], true);
        assert_eq!(json["capabilities"]["incremental"], false);
    }

    #[test]
    fn a_select_field_lists_its_options() {
        const SELECT: FieldSpec = FieldSpec {
            key: "mode",
            label: "Mode",
            help: "",
            kind: FieldKind::Select(&["a", "b"]),
            required: false,
            secret: false,
        };

        assert_eq!(field(&SELECT).options, ["a", "b"]);
        assert_eq!(field(&SELECT).kind, "select");
    }
}
