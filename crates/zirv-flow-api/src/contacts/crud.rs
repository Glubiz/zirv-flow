//! The contacts of a workspace: create, read, list, replace, patch and delete. A write applies
//! synchronously and is audited in its transaction; an `Idempotency-Key` makes a retry safe.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use utoipa::ToSchema;
use zirv_flow_core::WorkspaceId;
use zirv_flow_store::{Contact, ContactChange, Idempotency};

use super::{ContactBody, IdempotencyKey, Masking, store_error, write_error};
use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::jobs_admin::{DEFAULT_LIMIT, parse_limit};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};
use crate::trace::request_trace;

/// The body of `POST /contacts`.
#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateContactRequest {
    /// 1 to 256 characters without control characters; not `search` or `bulk`. It becomes the
    /// identifier `external:contact_key` and addresses the contact in URLs.
    #[schema(example = "c-1001")]
    key: String,
    /// The fields by registry key, checked against the contact definition.
    #[serde(default)]
    #[schema(value_type = Object)]
    fields: Map<String, Value>,
}

/// The body of `PUT` and `PATCH` `/contacts/{key}`.
#[derive(Deserialize, ToSchema)]
pub(crate) struct ContactFieldsRequest {
    /// For `PUT`, all the contact's fields. For `PATCH`, a JSON merge patch: a field set to
    /// `null` is removed, and removing a required field is refused with 422.
    #[schema(value_type = Object)]
    fields: Map<String, Value>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ContactsBody {
    /// In key order.
    contacts: Vec<ContactBody>,
    /// Pass as `after` for the next page; null on the last page.
    next_after: Option<String>,
}

const REPLAYED: HeaderName = HeaderName::from_static("idempotent-replayed");

/// What a write is, for the audit entry.
#[derive(Clone, Copy)]
enum Action {
    Created,
    Replaced,
    Patched,
    Deleted,
}

impl Action {
    const fn audit(self) -> &'static str {
        match self {
            Self::Created => "contact.created",
            Self::Replaced | Self::Patched => "contact.updated",
            Self::Deleted => "contact.deleted",
        }
    }

    const fn mode(self) -> &'static str {
        match self {
            Self::Created => "create",
            Self::Replaced => "replace",
            Self::Patched => "patch",
            Self::Deleted => "delete",
        }
    }
}

/// Applies one write and its audit entry in one transaction, then answers with the contact
/// as stored. A replay answers the first status and the contact as stored now, or `404` when
/// the contact has since gone; it writes and audits nothing.
async fn run(
    state: &AppState,
    principal: &Principal,
    workspace: WorkspaceId,
    key: &str,
    action: Action,
    change: Option<ContactChange>,
    idempotency: Option<IdempotencyKey>,
) -> Result<Response, ApiError> {
    let trace = request_trace();
    let remembered = idempotency.as_ref().map(|idempotency| Idempotency {
        key: &idempotency.key,
        request_hash: &idempotency.request_hash,
    });
    let mut tx = zirv_flow_store::begin(&state.pool)
        .await
        .map_err(store_error)?;
    let written = match change {
        Some(change) => {
            zirv_flow_store::write_contact(
                &mut tx,
                workspace,
                key,
                change,
                remembered.as_ref(),
                &trace,
            )
            .await
        }
        None => {
            zirv_flow_store::delete_contact(&mut tx, workspace, key, remembered.as_ref(), &trace)
                .await
        }
    }
    .map_err(write_error)?;
    if !written.replayed {
        admin::audit(
            &mut tx,
            &principal.context(),
            Some(workspace),
            action.audit(),
            written
                .contact
                .as_ref()
                .map(|contact| ("person", contact.person_id)),
            json!({ "mode": action.mode() }),
        )
        .await
        .map_err(store_error)?;
    }
    zirv_flow_store::commit(tx).await.map_err(store_error)?;

    let status = StatusCode::from_u16(written.status).unwrap_or(StatusCode::OK);
    let mut response = if status == StatusCode::NO_CONTENT {
        status.into_response()
    } else {
        let contact = written.contact.ok_or_else(ApiError::not_found)?;
        let masking = Masking::of(state, principal, workspace).await?;
        (status, Json(masking.body(contact))).into_response()
    };
    if written.replayed {
        response
            .headers_mut()
            .insert(REPLAYED, HeaderValue::from_static("true"));
    }
    Ok(response)
}

/// Creates a contact. The write applies in this request: the answer is the stored contact.
/// Send an `Idempotency-Key` header (1 to 255 characters) to make a retry safe: the same key
/// with the same request replays the first status and the contact as stored now, and writes
/// nothing; the same key with another request is refused with `422` and the code
/// `idempotency_key_reused`. Keys are remembered for 24 hours. Without the header, an
/// identical repeat is a no-op because a write that changes nothing stores nothing, but a
/// repeated create is answered `409`.
#[utoipa::path(
    post,
    operation_id = "create_contact",
    path = "/api/v1/workspaces/{ws}/contacts",
    tag = "contacts",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("Idempotency-Key" = Option<String>, Header, description = "1 to 255 characters; makes a retry safe for 24 hours")
    ),
    request_body = CreateContactRequest,
    responses(
        (status = 201, description = "The new contact", body = ContactBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it; or, on a replay, the contact has since been deleted", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "A contact with that key exists", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The fields break the contact definition or the field registry (errors lists field and reason, never values), or the Idempotency-Key is invalid or reused (`idempotency_key_reused`)", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The contact changed at the same moment; try again", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    ApiJson(body): ApiJson<Value>,
) -> Result<Response, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    let idempotency = IdempotencyKey::of(&headers, &method, &uri, &body)?;
    let request: CreateContactRequest = parse(body)?;
    run(
        &state,
        &principal,
        workspace.id,
        &request.key,
        Action::Created,
        Some(ContactChange::Create(request.fields)),
        idempotency,
    )
    .await
}

/// Parses an already-read body, with the fixed text of the JSON extractor.
fn parse<T: serde::de::DeserializeOwned>(body: Value) -> Result<T, ApiError> {
    serde_json::from_value(body).map_err(|_| {
        ApiError::unprocessable("request body is missing a field or has a field of the wrong type")
    })
}

/// One contact. Values of identifier fields and PII-flagged fields are masked for members and
/// `read` keys.
#[utoipa::path(
    get,
    operation_id = "get_contact",
    path = "/api/v1/workspaces/{ws}/contacts/{key}",
    tag = "contacts",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("key" = String, Path, description = "Contact key")
    ),
    responses(
        (status = 200, description = "The contact", body = ContactBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or contact, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, key)): Path<(String, String)>,
) -> Result<Json<ContactBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let mut connection = state
        .pool
        .acquire()
        .await
        .map_err(|_| ApiError::internal())?;
    let contact = zirv_flow_store::find_contact(&mut connection, workspace.id, &key)
        .await?
        .ok_or_else(ApiError::not_found)?;
    drop(connection);
    let masking = Masking::of(&state, &principal, workspace.id).await?;
    Ok(Json(masking.body(contact)))
}

fn encode_after(key: &str) -> String {
    URL_SAFE_NO_PAD.encode(key)
}

fn decode_after(cursor: &str) -> Option<String> {
    String::from_utf8(URL_SAFE_NO_PAD.decode(cursor).ok()?).ok()
}

fn parse_list_query(query: &str) -> Result<(u32, Option<String>), ApiError> {
    let (mut limit, mut after) = (DEFAULT_LIMIT, None);
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "limit" => limit = parse_limit(&value)?,
            "after" => {
                after = Some(
                    decode_after(&value)
                        .ok_or_else(|| ApiError::bad_request("after is not a valid cursor"))?,
                );
            }
            _ => {}
        }
    }
    Ok((limit, after))
}

/// The workspace's contacts in key order, keyset-paginated. For a lookup by key prefix, email
/// or phone use the search endpoints.
#[utoipa::path(
    get,
    operation_id = "list_contacts",
    path = "/api/v1/workspaces/{ws}/contacts",
    tag = "contacts",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("limit" = Option<u32>, Query, description = "Contacts per page, 1 to 200; default 50"),
        ("after" = Option<String>, Query, description = "The next_after of the previous page")
    ),
    responses(
        (status = 200, description = "A page of contacts", body = ContactsBody),
        (status = 400, description = "limit is out of range, or after is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn list(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<ContactsBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let (limit, after) = parse_list_query(query.as_deref().unwrap_or_default())?;
    let mut connection = state
        .pool
        .acquire()
        .await
        .map_err(|_| ApiError::internal())?;
    let page =
        zirv_flow_store::list_contacts(&mut connection, workspace.id, after.as_deref(), limit)
            .await?;
    drop(connection);
    let masking = Masking::of(&state, &principal, workspace.id).await?;
    Ok(Json(ContactsBody {
        next_after: page.next_after.as_deref().map(encode_after),
        contacts: page
            .contacts
            .into_iter()
            .map(|contact: Contact| masking.body(contact))
            .collect(),
    }))
}

/// Replaces all the fields of a contact. Required fields must be present. Supports the
/// `Idempotency-Key` header like `POST`.
#[utoipa::path(
    put,
    operation_id = "replace_contact",
    path = "/api/v1/workspaces/{ws}/contacts/{key}",
    tag = "contacts",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("key" = String, Path, description = "Contact key"),
        ("Idempotency-Key" = Option<String>, Header, description = "1 to 255 characters; makes a retry safe for 24 hours")
    ),
    request_body = ContactFieldsRequest,
    responses(
        (status = 200, description = "The contact as stored", body = ContactBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or contact, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The fields break the contact definition or the field registry, or the Idempotency-Key is invalid or reused", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The contact changed at the same moment; try again", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn replace(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, key)): Path<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    ApiJson(body): ApiJson<Value>,
) -> Result<Response, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    let idempotency = IdempotencyKey::of(&headers, &method, &uri, &body)?;
    let request: ContactFieldsRequest = parse(body)?;
    run(
        &state,
        &principal,
        workspace.id,
        &key,
        Action::Replaced,
        Some(ContactChange::Replace(request.fields)),
        idempotency,
    )
    .await
}

/// Changes some fields of a contact with a JSON merge patch over `fields`: a field set to
/// `null` is removed, and removing a required field is refused with 422. Supports the
/// `Idempotency-Key` header like `POST`.
#[utoipa::path(
    patch,
    operation_id = "patch_contact",
    path = "/api/v1/workspaces/{ws}/contacts/{key}",
    tag = "contacts",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("key" = String, Path, description = "Contact key"),
        ("Idempotency-Key" = Option<String>, Header, description = "1 to 255 characters; makes a retry safe for 24 hours")
    ),
    request_body = ContactFieldsRequest,
    responses(
        (status = 200, description = "The contact as stored", body = ContactBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or contact, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The result breaks the contact definition or the field registry, or the Idempotency-Key is invalid or reused", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The contact changed at the same moment; try again", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn patch(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, key)): Path<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    ApiJson(body): ApiJson<Value>,
) -> Result<Response, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    let idempotency = IdempotencyKey::of(&headers, &method, &uri, &body)?;
    let request: ContactFieldsRequest = parse(body)?;
    run(
        &state,
        &principal,
        workspace.id,
        &key,
        Action::Patched,
        Some(ContactChange::Patch(request.fields)),
        idempotency,
    )
    .await
}

/// Deletes a contact: its native record is retracted, and the workspace's orphan policy decides
/// whether the person goes too. Another source's data on the person stays. A repeat with the
/// same `Idempotency-Key` answers `204` again.
#[utoipa::path(
    delete,
    operation_id = "delete_contact",
    path = "/api/v1/workspaces/{ws}/contacts/{key}",
    tag = "contacts",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("key" = String, Path, description = "Contact key"),
        ("Idempotency-Key" = Option<String>, Header, description = "1 to 255 characters; makes a retry safe for 24 hours")
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or contact, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The Idempotency-Key is invalid or reused", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The contact changed at the same moment; try again", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn remove(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, key)): Path<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    let idempotency = IdempotencyKey::of(&headers, &method, &uri, &Value::Null)?;
    run(
        &state,
        &principal,
        workspace.id,
        &key,
        Action::Deleted,
        None,
        idempotency,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_round_trips_a_key_with_any_characters() {
        for key in ["c-1", "a/b?c=d&e", "ünï", "x y"] {
            assert_eq!(decode_after(&encode_after(key)).as_deref(), Some(key));
        }
        assert_eq!(decode_after("!!"), None);
    }

    #[test]
    fn the_list_query_takes_a_limit_and_an_opaque_after() {
        let after = encode_after("c-9");

        let parsed = parse_list_query(&format!("limit=5&after={after}")).unwrap();

        assert_eq!(parsed, (5, Some("c-9".to_owned())));
        assert_eq!(parse_list_query("").unwrap(), (DEFAULT_LIMIT, None));
        for bad in ["limit=0", "limit=201", "after=%21%21"] {
            assert_eq!(
                parse_list_query(bad).unwrap_err().status(),
                StatusCode::BAD_REQUEST,
                "{bad}"
            );
        }
    }

    #[test]
    fn a_write_audits_under_the_contact_actions() {
        assert_eq!(Action::Created.audit(), "contact.created");
        assert_eq!(Action::Replaced.audit(), "contact.updated");
        assert_eq!(Action::Patched.audit(), "contact.updated");
        assert_eq!(Action::Deleted.audit(), "contact.deleted");
        assert_eq!(Action::Patched.mode(), "patch");
    }
}
