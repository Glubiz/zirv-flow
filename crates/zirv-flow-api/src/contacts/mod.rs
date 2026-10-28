//! Native contacts (architecture section 7.6): the contact definition, a synchronous contacts
//! API with idempotency keys, and a bulk upsert through the ingestion inbox.
//!
//! A single-contact write applies in the request, through the profile path of apply-batch in
//! one transaction with its audit entry, and answers `201`, `200`, `204` or `422`. A bulk upsert
//! is validated at the edge and appended to the inbox (`202`); `ingest.drain` applies it.
//!
//! No log line, metric label, audit entry or problem body holds a field or an identifier
//! value. Problems name fields and reason codes.

#[cfg(test)]
mod acceptance;
pub(crate) mod bulk;
pub(crate) mod crud;
pub(crate) mod definition;
pub(crate) mod search;

use hashbrown::HashSet;

use axum::http::{HeaderMap, Method, StatusCode, Uri};
use serde::Serialize;
use serde_json::{Map, Value};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::contacts::FieldProblem;
use zirv_flow_core::token::sha256;
use zirv_flow_store::{Contact, StoreError, WriteError};

use crate::AppState;
use crate::auth::{Principal, may_view_pii};
use crate::problem::{ApiError, ItemProblem};

/// The most field names a problem's `detail` lists.
const DETAIL_FIELDS: usize = 5;

/// A contact. Values of identifier fields and PII-flagged fields are `null` and listed in
/// `masked_fields` for a caller who may not see values: members and `read` keys.
#[derive(Serialize, ToSchema)]
pub(crate) struct ContactBody {
    /// The contact key: the identifier `external:contact_key`.
    key: String,
    /// The person the contact belongs to; the merged profile is at `/people/{person_id}`.
    person_id: Uuid,
    /// The fields as written, by registry key.
    #[schema(value_type = std::collections::BTreeMap<String, serde_json::Value>)]
    fields: Map<String, Value>,
    /// The keys of `fields` whose value is hidden from this caller.
    masked_fields: Vec<String>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    updated_at: OffsetDateTime,
}

/// Which field values a caller may see.
pub(crate) struct Masking {
    /// None shows everything.
    hidden: Option<HashSet<String>>,
}

impl Masking {
    /// The masking of `principal` in the workspace: the owner, workspace admins and `admin`
    /// keys see every value; everyone else has identifier fields and PII-flagged fields hidden.
    pub(crate) async fn of(
        state: &AppState,
        principal: &Principal,
        workspace: WorkspaceId,
    ) -> Result<Self, ApiError> {
        if may_view_pii(state, principal, workspace).await {
            return Ok(Self { hidden: None });
        }
        let mut hidden: HashSet<String> =
            zirv_flow_store::list_trait_definitions(&state.pool, workspace)
                .await?
                .into_iter()
                .filter(|definition| definition.pii)
                .map(|definition| definition.key)
                .collect();
        if let Some(definition) =
            zirv_flow_store::find_contact_definition(&state.pool, workspace).await?
        {
            hidden.extend(
                definition
                    .fields
                    .into_iter()
                    .filter(|field| field.identifier.is_some())
                    .map(|field| field.key),
            );
        }
        Ok(Self {
            hidden: Some(hidden),
        })
    }

    pub(crate) fn body(&self, contact: Contact) -> ContactBody {
        let mut fields = Map::new();
        let mut masked_fields = Vec::new();
        for (key, value) in contact.fields {
            if self
                .hidden
                .as_ref()
                .is_some_and(|hidden| hidden.contains(&key))
            {
                masked_fields.push(key.clone());
                fields.insert(key, Value::Null);
            } else {
                fields.insert(key, value);
            }
        }
        ContactBody {
            key: contact.key,
            person_id: contact.person_id,
            fields,
            masked_fields,
            created_at: contact.created_at,
            updated_at: contact.updated_at,
        }
    }
}

/// The field problems as an RFC 9457 problem: `errors` lists every field, and the detail names
/// the first few.
pub(crate) fn invalid_fields(problems: Vec<FieldProblem>) -> ApiError {
    let named: Vec<&str> = problems
        .iter()
        .map(|problem| problem.field.as_str())
        .filter(|field| !field.is_empty())
        .take(DETAIL_FIELDS)
        .collect();
    let detail = if named.is_empty() {
        "the contact is invalid".to_owned()
    } else {
        format!("invalid fields: {}", named.join(", "))
    };
    ApiError::unprocessable(detail).with_errors(
        problems
            .into_iter()
            .map(|problem| ItemProblem {
                index: 0,
                field: problem.field,
                reason: problem.reason.to_owned(),
            })
            .collect(),
    )
}

pub(crate) fn write_error(error: WriteError) -> ApiError {
    match error {
        WriteError::Invalid(problems) => invalid_fields(problems),
        WriteError::Exists => ApiError::new(
            StatusCode::CONFLICT,
            "a contact with that key already exists",
        ),
        WriteError::NotFound => ApiError::not_found(),
        WriteError::KeyReused => ApiError::idempotency_key_reused(),
        WriteError::Store(error) => store_error(error),
    }
}

/// A race that outlasted its retries is worth retrying; anything else is an internal error.
pub(crate) fn store_error(error: StoreError) -> ApiError {
    if error.is_retryable() {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "the contact changed at the same moment, try again",
        )
    } else {
        error.into()
    }
}

/// An `Idempotency-Key` and the hash of the request it covers.
pub(crate) struct IdempotencyKey {
    pub(crate) key: String,
    pub(crate) request_hash: [u8; 32],
}

impl IdempotencyKey {
    /// The key of the request, if it sent one: 1 to 255 characters without control characters.
    /// The hash covers the method, the path and the body, parsed so equal JSON hashes equal.
    pub(crate) fn of(
        headers: &HeaderMap,
        method: &Method,
        uri: &Uri,
        body: &Value,
    ) -> Result<Option<Self>, ApiError> {
        let Some(key) = crate::idempotency::key_header(headers)? else {
            return Ok(None);
        };
        let request_hash = sha256(&format!("{method}\n{}\n{body}", uri.path()));
        Ok(Some(Self {
            key: key.to_owned(),
            request_hash,
        }))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn contact() -> Contact {
        Contact {
            key: "c-1".into(),
            person_id: Uuid::nil(),
            fields: json!({"email": "a@example.com", "plan": "pro", "ssn": "123"})
                .as_object()
                .cloned()
                .unwrap(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn hidden_fields_are_null_and_listed_and_the_rest_show() {
        let masking = Masking {
            hidden: Some(["email".to_owned(), "ssn".to_owned()].into()),
        };

        let body = masking.body(contact());

        assert_eq!(
            Value::Object(body.fields),
            json!({"email": null, "plan": "pro", "ssn": null})
        );
        assert_eq!(body.masked_fields, ["email", "ssn"]);
    }

    #[test]
    fn a_caller_who_may_view_pii_sees_all_of_them() {
        let body = Masking { hidden: None }.body(contact());

        assert_eq!(body.fields["email"], "a@example.com");
        assert!(body.masked_fields.is_empty());
    }

    #[test]
    fn invalid_fields_are_listed_and_the_detail_names_them_without_values() {
        let error = invalid_fields(vec![
            FieldProblem {
                field: "email".into(),
                reason: "missing_required",
            },
            FieldProblem {
                field: "age".into(),
                reason: "wrong_type",
            },
        ]);

        assert_eq!(error.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error.detail(), "invalid fields: email, age");
    }

    #[test]
    fn write_errors_map_to_their_statuses() {
        let status = |error| write_error(error).status();

        assert_eq!(status(WriteError::Exists), StatusCode::CONFLICT);
        assert_eq!(status(WriteError::NotFound), StatusCode::NOT_FOUND);
        assert_eq!(
            status(WriteError::KeyReused),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[test]
    fn an_idempotency_key_is_one_to_255_characters_and_its_hash_covers_the_request() {
        let uri: Uri = "/api/v1/workspaces/w/contacts".parse().unwrap();
        let with = |key: &str, body: Value| {
            let mut headers = HeaderMap::new();
            headers.insert("idempotency-key", key.parse().unwrap());
            IdempotencyKey::of(&headers, &Method::POST, &uri, &body)
        };

        let a = with("k", json!({"a": 1, "b": 2})).unwrap().unwrap();
        let reordered = with("k", json!({"b": 2, "a": 1})).unwrap().unwrap();
        let other = with("k", json!({"a": 2})).unwrap().unwrap();

        assert_eq!(a.request_hash, reordered.request_hash);
        assert_ne!(a.request_hash, other.request_hash);
        assert!(with(&"x".repeat(256), json!({})).is_err());
        assert!(
            IdempotencyKey::of(&HeaderMap::new(), &Method::POST, &uri, &json!({}))
                .unwrap()
                .is_none()
        );
    }
}
