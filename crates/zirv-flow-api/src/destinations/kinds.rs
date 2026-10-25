//! The destination types this build ships, with the fields the setup form renders. It reads the
//! destinations registry: there is no second list.

use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;
use zirv_flow_connectors::destinations::DestinationSpec;

use super::registry;
use crate::auth::Principal;
use crate::problem::{ApiError, Problem};
use crate::sources::kinds::{FieldSpecBody, field};

#[derive(Serialize, ToSchema)]
pub(crate) struct DestinationCapabilitiesBody {
    /// The channels it carries, such as `email`, `webhook` or `event`. A forwarding rule may name
    /// a destination whose type carries `event`.
    channels: Vec<String>,
    idempotency_key: bool,
    batching: bool,
    /// Whether its destinations are rate limited by the shared bucket.
    rate_limit: bool,
    erase: bool,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct DestinationKindBody {
    /// The stable id to send as `kind` when creating a destination.
    #[schema(example = "webhook")]
    kind: String,
    label: String,
    description: String,
    /// Plain settings, stored as the destination's configuration.
    config_fields: Vec<FieldSpecBody>,
    /// Settings held only in the workspace credential the destination uses.
    credential_fields: Vec<FieldSpecBody>,
    capabilities: DestinationCapabilitiesBody,
}

impl From<&DestinationSpec> for DestinationKindBody {
    fn from(spec: &DestinationSpec) -> Self {
        Self {
            kind: spec.kind.to_owned(),
            label: spec.label.to_owned(),
            description: spec.description.to_owned(),
            config_fields: spec.config_fields.iter().map(field).collect(),
            credential_fields: spec.credential_fields.iter().map(field).collect(),
            capabilities: DestinationCapabilitiesBody {
                channels: spec
                    .capabilities
                    .channels
                    .iter()
                    .map(|channel| (*channel).to_owned())
                    .collect(),
                idempotency_key: spec.capabilities.idempotency_key,
                batching: spec.capabilities.batching,
                rate_limit: spec.capabilities.rate_limit,
                erase: spec.capabilities.erase,
            },
        }
    }
}

/// The destination types this build ships, with their field specs. Any signed-in user or API key.
#[utoipa::path(
    get,
    operation_id = "list_destination_kinds",
    path = "/api/v1/destination-kinds",
    tag = "destinations",
    responses(
        (status = 200, description = "The destination types", body = Vec<DestinationKindBody>),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn list(
    _principal: Principal,
) -> Result<Json<Vec<DestinationKindBody>>, ApiError> {
    Ok(Json(
        registry()
            .specs()
            .into_iter()
            .map(DestinationKindBody::from)
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    #[test]
    fn every_registered_kind_is_listed_with_its_secret_fields_marked() {
        let bodies: Vec<_> = registry()
            .specs()
            .into_iter()
            .map(DestinationKindBody::from)
            .collect();

        let kinds: Vec<_> = bodies.iter().map(|body| body.kind.as_str()).collect();
        assert_eq!(kinds, ["webhook", "smtp", "provider", "fake"]);
        let json = serde_json::to_value(&bodies).unwrap();
        let webhook = &json[0];
        assert_eq!(webhook["config_fields"][0]["key"], "url");
        assert_eq!(webhook["config_fields"][0]["kind"], "url");
        assert_eq!(webhook["config_fields"][0]["required"], true);
        assert_eq!(webhook["credential_fields"][0]["key"], "signing_secret");
        assert_eq!(webhook["credential_fields"][0]["required"], true);
        assert_eq!(
            webhook["capabilities"]["channels"],
            serde_json::json!(["webhook", "event"])
        );
        let smtp = &json[1];
        assert_eq!(
            smtp["capabilities"]["channels"],
            serde_json::json!(["email"])
        );
        assert_eq!(smtp["capabilities"]["rate_limit"], true);
        let secret_of = |kind: &Value, key: &str| {
            kind["credential_fields"]
                .as_array()
                .unwrap()
                .iter()
                .find(|field| field["key"] == key)
                .map(|field| field["secret"].clone())
        };
        assert_eq!(secret_of(webhook, "signing_secret"), Some(true.into()));
        assert_eq!(secret_of(smtp, "password"), Some(true.into()));
        assert_eq!(secret_of(smtp, "host"), Some(false.into()));
    }
}
