//! Contact search: by contact key prefix, or by an email or phone matched exactly after
//! normalisation. The POST keeps the value out of URLs and access logs.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::people::Identifier;
use zirv_flow_store::{Contact, ContactSearch, ContactSearchError};

use super::{ContactBody, Masking};
use crate::AppState;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::jobs_admin::{DEFAULT_LIMIT, MAX_LIMIT, parse_limit};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

const MAX_QUERY_CHARS: usize = 256;

/// An identifier to find: the value is normalised for the kind, then matched exactly.
#[derive(Deserialize, ToSchema)]
pub(crate) struct ContactIdentifierQuery {
    /// `email`, `phone`, `anonymous_id` or `external:<name>`.
    #[schema(example = "email")]
    kind: String,
    value: String,
}

/// Exactly one of `identifier` and `q`.
#[derive(Deserialize, ToSchema)]
pub(crate) struct SearchContactsRequest {
    identifier: Option<ContactIdentifierQuery>,
    /// As the `q` of the GET: contacts whose key starts with it, and the contacts of the
    /// person with that email or phone.
    q: Option<String>,
    /// Contacts to return, 1 to 200; default 50.
    limit: Option<u32>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SearchContactsBody {
    /// In key order; at most `limit`.
    contacts: Vec<ContactBody>,
}

fn check_query(q: &str) -> Result<(), ApiError> {
    if (1..=MAX_QUERY_CHARS).contains(&q.chars().count()) {
        Ok(())
    } else {
        Err(ApiError::unprocessable("q: must be 1 to 256 characters"))
    }
}

fn search_error(error: ContactSearchError) -> ApiError {
    match error {
        ContactSearchError::InvalidIdentifier(reason) => {
            ApiError::unprocessable(format!("identifier: {reason}"))
        }
        ContactSearchError::Store(error) => error.into(),
    }
}

/// The contacts whose key starts with `q`, plus those of the person that holds `q` as an email
/// or a phone number, in key order.
async fn find(
    state: &AppState,
    workspace: WorkspaceId,
    q: &str,
    limit: u32,
) -> Result<Vec<Contact>, ApiError> {
    let mut connection = state
        .pool
        .acquire()
        .await
        .map_err(|_| ApiError::internal())?;
    let mut found = zirv_flow_store::search_contacts(
        &mut connection,
        workspace,
        &ContactSearch::KeyPrefix(q.to_owned()),
        limit,
    )
    .await
    .map_err(search_error)?;
    for kind in ["email", "phone"] {
        if Identifier::parse(kind, q).is_err() {
            continue;
        }
        let by_identifier = zirv_flow_store::search_contacts(
            &mut connection,
            workspace,
            &ContactSearch::Identifier {
                kind: kind.to_owned(),
                value: q.to_owned(),
            },
            limit,
        )
        .await
        .map_err(search_error)?;
        found.extend(by_identifier);
    }
    found.sort_by(|a, b| a.key.cmp(&b.key));
    found.dedup_by(|a, b| a.key == b.key);
    found.truncate(limit as usize);
    Ok(found)
}

/// Finds contacts by key prefix, or by an exact email or phone number. The value is in the URL
/// and so in access logs: prefer the POST for addresses and numbers.
#[utoipa::path(
    get,
    operation_id = "search_contacts_get",
    path = "/api/v1/workspaces/{ws}/contacts/search",
    tag = "contacts",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("q" = String, Query, description = "A contact key prefix, an email or a phone number; 1 to 256 characters"),
        ("limit" = Option<u32>, Query, description = "Contacts to return, 1 to 200; default 50")
    ),
    responses(
        (status = 200, description = "The matching contacts", body = SearchContactsBody),
        (status = 400, description = "limit is out of range", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "q is missing or too long", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<SearchContactsBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let (mut q, mut limit) = (None, DEFAULT_LIMIT);
    for (key, value) in url::form_urlencoded::parse(query.as_deref().unwrap_or_default().as_bytes())
    {
        match key.as_ref() {
            "q" => q = Some(value.into_owned()),
            "limit" => limit = parse_limit(&value)?,
            _ => {}
        }
    }
    let q = q.ok_or_else(|| ApiError::unprocessable("q: is required"))?;
    check_query(&q)?;
    let contacts = find(&state, workspace.id, &q, limit).await?;
    reply(&state, &principal, workspace.id, contacts).await
}

async fn reply(
    state: &AppState,
    principal: &Principal,
    workspace: WorkspaceId,
    contacts: Vec<Contact>,
) -> Result<Json<SearchContactsBody>, ApiError> {
    let masking = Masking::of(state, principal, workspace).await?;
    Ok(Json(SearchContactsBody {
        contacts: contacts
            .into_iter()
            .map(|contact| masking.body(contact))
            .collect(),
    }))
}

/// Finds contacts by an identifier matched exactly after normalisation, or by `q` like the GET.
/// A POST so the value stays out of URLs and access logs.
#[utoipa::path(
    post,
    operation_id = "search_contacts",
    path = "/api/v1/workspaces/{ws}/contacts/search",
    tag = "contacts",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = SearchContactsRequest,
    responses(
        (status = 200, description = "The matching contacts", body = SearchContactsBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Not exactly one of identifier and q, an invalid identifier, or a limit out of range", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn post(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<SearchContactsRequest>,
) -> Result<Json<SearchContactsBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let limit = match body.limit {
        None => DEFAULT_LIMIT,
        Some(limit) if (1..=MAX_LIMIT).contains(&limit) => limit,
        Some(_) => {
            return Err(ApiError::unprocessable(format!(
                "limit: must be from 1 to {MAX_LIMIT}"
            )));
        }
    };
    let contacts = match (body.identifier, body.q) {
        (Some(identifier), None) => {
            let mut connection = state
                .pool
                .acquire()
                .await
                .map_err(|_| ApiError::internal())?;
            zirv_flow_store::search_contacts(
                &mut connection,
                workspace.id,
                &ContactSearch::Identifier {
                    kind: identifier.kind,
                    value: identifier.value,
                },
                limit,
            )
            .await
            .map_err(search_error)?
        }
        (None, Some(q)) => {
            check_query(&q)?;
            find(&state, workspace.id, &q, limit).await?
        }
        _ => {
            return Err(ApiError::unprocessable(
                "search: give exactly one of identifier and q",
            ));
        }
    };
    reply(&state, &principal, workspace.id, contacts).await
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::*;

    #[test]
    fn a_query_is_one_to_256_characters() {
        assert!(check_query("a").is_ok());
        assert!(check_query(&"x".repeat(256)).is_ok());
        for bad in [String::new(), "x".repeat(257)] {
            assert_eq!(
                check_query(&bad).unwrap_err().status(),
                StatusCode::UNPROCESSABLE_ENTITY
            );
        }
    }

    #[test]
    fn an_invalid_identifier_is_a_422_that_names_the_rule_and_not_the_value() {
        let error = search_error(ContactSearchError::InvalidIdentifier("invalid_email"));

        assert_eq!(error.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error.detail(), "identifier: invalid_email");
    }
}
