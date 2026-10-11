//! Bulk upsert of contacts (architecture section 7.6): validated at the edge against the contact
//! definition, charged against the ingest rate limit, appended to the durable inbox and
//! answered `202`. `ingest.drain` applies the items, as it does for identify.

use axum::Json;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{Map, Value, json};
use utoipa::ToSchema;
use zirv_flow_core::contacts::{FieldProblem, TypedField, plan, validate_contact_key};
use zirv_flow_store::{IngestItem, IngestPayload};

use super::store_error;
use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::ingest::{read_json, unavailable};
use crate::problem::{ApiError, ItemProblem, Problem};
use crate::trace::request_trace;

/// One contact of a bulk upsert.
#[derive(ToSchema)]
#[allow(
    dead_code,
    reason = "describes the request in OpenAPI; the body is parsed by hand so errors name items"
)]
pub(crate) struct BulkContactItem {
    /// The contact key.
    #[schema(example = "c-1001")]
    key: String,
    /// All the contact's fields: an upsert replaces the fields of an existing contact.
    #[schema(value_type = std::collections::BTreeMap<String, serde_json::Value>)]
    fields: Option<Map<String, Value>>,
}

/// The body of `POST /contacts/bulk`.
#[derive(ToSchema)]
#[allow(
    dead_code,
    reason = "describes the request in OpenAPI; the body is parsed by hand so errors name items"
)]
pub(crate) struct BulkContactsRequest {
    /// At least one contact, at most `ingest.max_batch_items`. All are validated before any is
    /// appended; a key listed twice is applied in order.
    contacts: Vec<BulkContactItem>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct BulkAcceptedBody {
    /// Contacts appended to the inbox: durable, not yet applied.
    accepted: usize,
}

fn problem(index: usize, field: &str, reason: &str) -> ItemProblem {
    ItemProblem {
        index: u32::try_from(index).unwrap_or(u32::MAX),
        field: field.to_owned(),
        reason: reason.to_owned(),
    }
}

fn invalid(errors: Vec<ItemProblem>) -> ApiError {
    ApiError::unprocessable("one or more contacts are invalid").with_errors(errors)
}

/// Validates the items of a bulk request against `definition` and maps them to inbox items.
/// Every invalid item is reported, by index, field and reason.
fn items(
    body: &Value,
    definition: Option<&[TypedField]>,
) -> Result<Vec<IngestItem>, Vec<ItemProblem>> {
    let list = contacts_of(body)?;
    let mut errors = Vec::new();
    let mut accepted = Vec::with_capacity(list.len());
    for (index, entry) in list.iter().enumerate() {
        let Some(entry) = entry.as_object() else {
            errors.push(problem(index, "", "wrong_type"));
            continue;
        };
        let key = match entry.get("key").and_then(Value::as_str) {
            Some(key) if validate_contact_key(key).is_ok() => key,
            Some(_) => {
                errors.push(problem(index, "key", "invalid_key"));
                continue;
            }
            None => {
                errors.push(problem(index, "key", "required"));
                continue;
            }
        };
        let fields = match entry.get("fields") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(fields)) => fields.clone(),
            Some(_) => {
                errors.push(problem(index, "fields", "wrong_type"));
                continue;
            }
        };
        match plan(definition, key, &fields) {
            Ok(planned) => accepted.push(IngestItem {
                identifiers: planned.identifiers,
                timestamp: None,
                message_id: None,
                payload: IngestPayload::Contact {
                    traits: planned.traits,
                },
            }),
            Err(found) => errors.extend(
                found
                    .iter()
                    .map(|FieldProblem { field, reason }| problem(index, field, reason)),
            ),
        }
    }
    if errors.is_empty() {
        Ok(accepted)
    } else {
        Err(errors)
    }
}

/// The `contacts` array of the body, which must be an object.
fn contacts_of(body: &Value) -> Result<&Vec<Value>, Vec<ItemProblem>> {
    body.get("contacts")
        .and_then(Value::as_array)
        .ok_or_else(|| vec![problem(0, "contacts", "required")])
}

/// Upserts many contacts through the ingestion inbox: all are validated against the contact
/// definition, then appended, and `202` is returned once the append has committed. The drain
/// applies them like an identify, replacing the fields of an existing contact. Use the
/// single-contact endpoints when the answer must be the stored contact. Needs the write scope;
/// a session may use it too, with the CSRF token. Sending is limited per API key (per user for
/// a session) at the ingestion rate, and the body at the ingestion size.
#[utoipa::path(
    post,
    operation_id = "bulk_upsert_contacts",
    path = "/api/v1/workspaces/{ws}/contacts/bulk",
    tag = "contacts",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = BulkContactsRequest,
    responses(
        (status = 202, description = "Appended to the inbox: durable, not yet applied", body = BulkAcceptedBody),
        (status = 400, description = "The body is not valid JSON", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "The body is over ingest.max_request_bytes", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "No contacts, too many, or an invalid one; errors lists each by index, field and reason (never values), and nothing is appended", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Over the rate limit", body = Problem, content_type = "application/problem+json",
            headers(("Retry-After" = u64, description = "Whole seconds until the request's contacts fit"))),
        (status = 503, description = "The database is unavailable; nothing was stored", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn bulk(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    let limits = state.ingest.limits();
    let max_items = limits.max_batch_items;
    let body = read_json(request, limits.max_request_bytes).await?;

    let count = contacts_of(&body).map_err(invalid)?.len();
    if count == 0 || count > max_items {
        return Err(ApiError::unprocessable(format!(
            "contacts: must list 1 to {max_items} contacts"
        ))
        .with_errors(vec![problem(
            0,
            "contacts",
            if count == 0 { "empty" } else { "too_many" },
        )]));
    }
    let sender = match &principal {
        Principal::Key(key) => key.key.id.as_uuid(),
        Principal::Session(session) => session.user.id.as_uuid(),
    };
    state.ingest.charge(sender, count)?;

    let mut connection = state
        .pool
        .acquire()
        .await
        .map_err(|_| ApiError::internal())?;
    let definition = zirv_flow_store::typed_definition(&mut connection, workspace.id).await?;
    drop(connection);
    let items = items(&body, definition.as_deref()).map_err(invalid)?;

    zirv_flow_store::append_inbox(&state.pool, workspace.id, sender, &request_trace(), &items)
        .await
        .map_err(|_| unavailable())?;
    metrics::counter!("zirv_flow_ingest_items_total", "kind" => "contact")
        .increment(items.len() as u64);
    let mut tx = zirv_flow_store::begin(&state.pool)
        .await
        .map_err(store_error)?;
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "contact.bulk_upserted",
        None,
        json!({ "count": items.len() }),
    )
    .await
    .map_err(store_error)?;
    zirv_flow_store::commit(tx).await.map_err(store_error)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(BulkAcceptedBody {
            accepted: items.len(),
        }),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use zirv_flow_core::contacts::ContactField;
    use zirv_flow_core::people::TraitType;

    use super::*;

    fn definition() -> Vec<TypedField> {
        vec![
            TypedField {
                field: ContactField {
                    key: "email".into(),
                    required: true,
                    identifier: Some("email".into()),
                },
                trait_type: TraitType::String,
            },
            TypedField {
                field: ContactField {
                    key: "age".into(),
                    required: false,
                    identifier: None,
                },
                trait_type: TraitType::Number,
            },
        ]
    }

    fn reasons(errors: &[ItemProblem]) -> Vec<(u32, &str, &str)> {
        errors
            .iter()
            .map(|e| (e.index, e.field.as_str(), e.reason.as_str()))
            .collect()
    }

    #[test]
    fn valid_contacts_become_inbox_items_with_the_key_first() {
        let body = json!({"contacts": [
            {"key": "c-1", "fields": {"email": "A@Example.com", "age": 3}},
            {"key": "c-2", "fields": {"email": "b@example.com"}}
        ]});

        let items = items(&body, Some(&definition())).unwrap();

        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0].identifiers,
            [
                ("external:contact_key".to_owned(), "c-1".to_owned()),
                ("email".to_owned(), "A@Example.com".to_owned())
            ]
        );
        assert!(matches!(
            &items[0].payload,
            IngestPayload::Contact { traits } if traits["age"] == 3
        ));
    }

    #[test]
    fn every_invalid_contact_is_reported_by_index_field_and_reason() {
        let body = json!({"contacts": [
            {"key": "ok", "fields": {"email": "a@example.com"}},
            {"key": "c-2", "fields": {"age": "secret"}},
            {"fields": {}},
            {"key": "search", "fields": {"email": "a@example.com"}},
            {"key": "c-5", "fields": [1]},
            7
        ]});

        let errors = items(&body, Some(&definition())).unwrap_err();

        assert_eq!(
            reasons(&errors),
            [
                (1, "email", "missing_required"),
                (1, "age", "wrong_type"),
                (2, "key", "required"),
                (3, "key", "invalid_key"),
                (4, "fields", "wrong_type"),
                (5, "", "wrong_type"),
            ]
        );
        assert!(!format!("{errors:?}").contains("secret"));
    }

    #[test]
    fn without_a_definition_any_valid_key_is_accepted_and_contacts_may_have_no_fields() {
        let body = json!({"contacts": [{"key": "c-1"}, {"key": "c-2", "fields": {"plan": "pro"}}]});

        assert_eq!(items(&body, None).unwrap().len(), 2);
    }

    #[test]
    fn a_body_without_a_contacts_array_is_refused() {
        let errors = items(&json!({"contact": []}), None).unwrap_err();

        assert_eq!(reasons(&errors), [(0, "contacts", "required")]);
    }
}
