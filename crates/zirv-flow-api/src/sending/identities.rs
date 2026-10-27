//! The sender identities of a workspace: create on a sending domain, read, change and archive. An
//! identity's from address must be on its domain. Everything here is for workspace admins, and
//! every change is audited with ids and addresses of the workspace's own senders.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::{Email, UserId, WorkspaceId};
use zirv_flow_store::{
    IdentityError, NewSenderIdentity, PgPool, SenderIdentity, SenderIdentityPatch,
};

use crate::AppState;
use crate::admin::{self, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::consent::bool_param;
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};
use crate::sources::invalid;

const MAX_NAME_CHARS: usize = 128;

/// A sender identity.
#[derive(Serialize, ToSchema)]
pub(crate) struct SenderIdentityBody {
    id: Uuid,
    /// The sending domain the address is on.
    domain_id: Uuid,
    #[schema(example = "Example News")]
    from_name: String,
    #[schema(example = "news@example.com")]
    from_address: String,
    reply_to: Option<String>,
    /// An archived identity is read-only history and cannot send.
    archived: bool,
    /// The user who created it; null when an API key did.
    created_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    updated_at: OffsetDateTime,
}

impl From<&SenderIdentity> for SenderIdentityBody {
    fn from(identity: &SenderIdentity) -> Self {
        Self {
            id: identity.id,
            domain_id: identity.domain_id,
            from_name: identity.from_name.clone(),
            from_address: identity.from_address.clone(),
            reply_to: identity.reply_to.clone(),
            archived: identity.archived_at.is_some(),
            created_by: identity.created_by,
            created_at: identity.created_at,
            updated_at: identity.updated_at,
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateSenderIdentityRequest {
    /// A sending domain of this workspace.
    domain_id: Uuid,
    /// The display name, 1 to 128 characters.
    #[schema(example = "Example News")]
    from_name: String,
    /// An address on the sending domain.
    #[schema(example = "news@example.com")]
    from_address: String,
    /// Where replies go, if not to the from address. May be on any domain.
    reply_to: Option<String>,
}

/// Tells an absent field from a `null` one: absent is `None`, `null` is `Some(None)`.
fn present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UpdateSenderIdentityRequest {
    from_name: Option<String>,
    /// An address on the identity's domain.
    from_address: Option<String>,
    /// A new reply-to address; `null` clears it.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    reply_to: Option<Option<String>>,
}

fn check_from_name(name: &str) -> Result<&str, ApiError> {
    let name = name.trim();
    let length = name.chars().count();
    if length == 0 || length > MAX_NAME_CHARS || name.chars().any(char::is_control) {
        Err(invalid(
            "from_name",
            "must be 1 to 128 characters without control characters",
        ))
    } else {
        Ok(name)
    }
}

fn check_address(field: &str, value: &str) -> Result<Email, ApiError> {
    Email::parse(value).map_err(|_| invalid(field, "is not a valid email address"))
}

fn identity_error(error: IdentityError) -> ApiError {
    match error {
        IdentityError::NotFound => ApiError::not_found(),
        IdentityError::OffDomain => invalid(
            "from_address",
            "must be an address on the identity's sending domain",
        ),
        IdentityError::Taken => ApiError::new(
            StatusCode::CONFLICT,
            "a sender identity with that address already exists",
        ),
        IdentityError::Store(error) => error.into(),
    }
}

fn archived() -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "this sender identity is archived")
        .with_code("sender_identity_archived")
}

/// Creates an identity on one of the workspace's sending domains and audits
/// `sender_identity.created`.
pub(crate) async fn create_sender_identity(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    created_by: Option<UserId>,
    request: &CreateSenderIdentityRequest,
) -> Result<SenderIdentity, ApiError> {
    let from_name = check_from_name(&request.from_name)?;
    let from_address = check_address("from_address", &request.from_address)?;
    let reply_to = request
        .reply_to
        .as_deref()
        .map(|value| check_address("reply_to", value))
        .transpose()?;
    let mut tx = zirv_flow_store::begin(pool).await?;
    let identity = zirv_flow_store::create_sender_identity_in(
        &mut tx,
        workspace,
        &NewSenderIdentity {
            id: Uuid::now_v7(),
            domain_id: request.domain_id,
            from_name,
            from_address: &from_address,
            reply_to: reply_to.as_ref(),
            created_by,
        },
    )
    .await
    .map_err(|error| match error {
        // The domain is a field of the body, not part of the path.
        IdentityError::NotFound => {
            invalid("domain_id", "is not a sending domain of this workspace")
        }
        other => identity_error(other),
    })?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "sender_identity.created",
        Some(("sender_identity", identity.id)),
        json!({"domain_id": identity.domain_id}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(identity)
}

/// Changes a live identity and audits `sender_identity.updated` with the changed field names.
pub(crate) async fn update_sender_identity(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
    request: &UpdateSenderIdentityRequest,
) -> Result<SenderIdentity, ApiError> {
    let mut fields: Vec<&'static str> = Vec::new();
    let from_name = request
        .from_name
        .as_deref()
        .map(check_from_name)
        .transpose()?;
    if from_name.is_some() {
        fields.push("from_name");
    }
    let from_address = request
        .from_address
        .as_deref()
        .map(|value| check_address("from_address", value))
        .transpose()?;
    if from_address.is_some() {
        fields.push("from_address");
    }
    let reply_to = match &request.reply_to {
        None => None,
        Some(None) => Some(None),
        Some(Some(value)) => Some(Some(check_address("reply_to", value)?)),
    };
    if reply_to.is_some() {
        fields.push("reply_to");
    }
    if fields.is_empty() {
        return Err(invalid("body", "must change at least one field"));
    }
    let mut tx = zirv_flow_store::begin(pool).await?;
    let existing = zirv_flow_store::find_sender_identity(&mut *tx, workspace, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if existing.archived_at.is_some() {
        return Err(archived());
    }
    let identity = zirv_flow_store::update_sender_identity(
        &mut tx,
        workspace,
        id,
        &SenderIdentityPatch {
            from_name,
            from_address: from_address.as_ref(),
            reply_to: reply_to.as_ref().map(Option::as_ref),
        },
    )
    .await
    .map_err(identity_error)?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "sender_identity.updated",
        Some(("sender_identity", identity.id)),
        json!({"fields": fields}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(identity)
}

/// Archives a live identity and audits `sender_identity.archived`. Its address is free again;
/// templates that name it can no longer be published.
pub(crate) async fn archive_sender_identity(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<(), ApiError> {
    let mut tx = zirv_flow_store::begin(pool).await?;
    let identity = match zirv_flow_store::archive_sender_identity(&mut *tx, workspace, id).await? {
        Some(identity) => identity,
        None => {
            return Err(
                match zirv_flow_store::find_sender_identity(&mut *tx, workspace, id).await? {
                    Some(_) => archived(),
                    None => ApiError::not_found(),
                },
            );
        }
    };
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "sender_identity.archived",
        Some(("sender_identity", identity.id)),
        json!({"archived": true}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(())
}

/// One sender identity a writer may pick: the display name and address, no domain settings.
#[derive(Serialize, ToSchema)]
pub(crate) struct SenderIdentityOptionBody {
    id: Uuid,
    /// The display name.
    name: String,
    #[schema(example = "news@example.com")]
    from_address: String,
    /// An archived identity is listed so a template that names it still shows its name; it
    /// cannot send.
    archived: bool,
}

/// The workspace's sender identities as picker options, archived ones included and marked, by
/// address. Needs the write role: this is what a writer picks a template's sender from.
#[utoipa::path(
    get,
    operation_id = "list_sender_identity_options",
    path = "/api/v1/workspaces/{ws}/sender-identities/options",
    tag = "sending",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The options: id, name, address and state, nothing else", body = Vec<SenderIdentityOptionBody>),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn options(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Json<Vec<SenderIdentityOptionBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Edit).await?;
    let identities =
        zirv_flow_store::list_sender_identities(&state.pool, workspace.id, None, true).await?;
    Ok(Json(
        identities
            .iter()
            .map(|identity| SenderIdentityOptionBody {
                id: identity.id,
                name: identity.from_name.clone(),
                from_address: identity.from_address.clone(),
                archived: identity.archived_at.is_some(),
            })
            .collect(),
    ))
}

/// The workspace's live sender identities by address; archived ones with `archived=true`, and
/// only those of one domain with `domain_id`. Workspace admins only.
#[utoipa::path(
    get,
    operation_id = "list_sender_identities",
    path = "/api/v1/workspaces/{ws}/sender-identities",
    tag = "sending",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("archived" = Option<bool>, Query, description = "Include archived identities; default false"),
        ("domain_id" = Option<Uuid>, Query, description = "Only identities on this sending domain")
    ),
    responses(
        (status = 200, description = "The sender identities", body = Vec<SenderIdentityBody>),
        (status = 400, description = "archived is not true or false, or domain_id is not an id", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or a missing scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn list(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<Vec<SenderIdentityBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let query = query.unwrap_or_default();
    let include_archived = bool_param(&query, "archived")?;
    let mut domain = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if key == "domain_id" {
            domain = Some(
                value
                    .parse::<Uuid>()
                    .map_err(|_| ApiError::bad_request("domain_id must be an id"))?,
            );
        }
    }
    let identities = zirv_flow_store::list_sender_identities(
        &state.pool,
        workspace.id,
        domain,
        include_archived,
    )
    .await?;
    Ok(Json(
        identities.iter().map(SenderIdentityBody::from).collect(),
    ))
}

/// Creates a sender identity on a sending domain of the workspace. The from address must be on
/// the domain. Workspace admins only. Audited as `sender_identity.created`.
#[utoipa::path(
    post,
    operation_id = "create_sender_identity",
    path = "/api/v1/workspaces/{ws}/sender-identities",
    tag = "sending",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = CreateSenderIdentityRequest,
    responses(
        (status = 201, description = "The new identity", body = SenderIdentityBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "A live identity already has that address", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid name or address, an unknown domain, or an address off the domain", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<CreateSenderIdentityRequest>,
) -> Result<(StatusCode, Json<SenderIdentityBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let identity = create_sender_identity(
        &state.pool,
        &principal.context(),
        workspace.id,
        principal.user_id(),
        &body,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(SenderIdentityBody::from(&identity)),
    ))
}

/// One sender identity, archived or not. Workspace admins only.
#[utoipa::path(
    get,
    operation_id = "get_sender_identity",
    path = "/api/v1/workspaces/{ws}/sender-identities/{id}",
    tag = "sending",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Sender identity id")
    ),
    responses(
        (status = 200, description = "The identity", body = SenderIdentityBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or a missing scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or identity, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<SenderIdentityBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let identity = zirv_flow_store::find_sender_identity(&state.pool, workspace.id, path_id(&id)?)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(SenderIdentityBody::from(&identity)))
}

/// Changes a live sender identity: name, from address (on the same domain) or reply-to. Workspace
/// admins only. Audited as `sender_identity.updated`.
#[utoipa::path(
    patch,
    operation_id = "update_sender_identity",
    path = "/api/v1/workspaces/{ws}/sender-identities/{id}",
    tag = "sending",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Sender identity id")
    ),
    request_body = UpdateSenderIdentityRequest,
    responses(
        (status = 200, description = "The updated identity", body = SenderIdentityBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or identity, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Another live identity has that address, or the identity is archived (`code` is `sender_identity_archived`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid field, an address off the domain, or no field at all", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn update(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<UpdateSenderIdentityRequest>,
) -> Result<Json<SenderIdentityBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let identity = update_sender_identity(
        &state.pool,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
        &body,
    )
    .await?;
    Ok(Json(SenderIdentityBody::from(&identity)))
}

/// Archives a sender identity: it is no longer listed, changed or accepted at publish, and its
/// address is free again. Workspace admins only. Audited as `sender_identity.archived`.
#[utoipa::path(
    delete,
    operation_id = "archive_sender_identity",
    path = "/api/v1/workspaces/{ws}/sender-identities/{id}",
    tag = "sending",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Sender identity id")
    ),
    responses(
        (status = 204, description = "Archived"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or identity, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The identity is already archived (`code` is `sender_identity_archived`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn remove(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    archive_sender_identity(
        &state.pool,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_from_name_is_trimmed_and_bounded() {
        assert_eq!(check_from_name("  News  ").unwrap(), "News");
        assert!(check_from_name("   ").is_err());
        assert!(check_from_name("a\nb").is_err());
        assert!(check_from_name(&"x".repeat(129)).is_err());
        assert!(check_from_name(&"x".repeat(128)).is_ok());
    }

    #[test]
    fn an_address_that_is_not_one_is_refused_with_its_field() {
        let error = check_address("reply_to", "nope").unwrap_err();

        assert_eq!(error.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(error.detail().contains("reply_to"));
    }

    #[test]
    fn an_update_tells_an_absent_reply_to_from_a_null_one() {
        let absent: UpdateSenderIdentityRequest = serde_json::from_str("{}").unwrap();
        let cleared: UpdateSenderIdentityRequest =
            serde_json::from_str(r#"{"reply_to":null}"#).unwrap();
        let set: UpdateSenderIdentityRequest =
            serde_json::from_str(r#"{"reply_to":"a@example.com"}"#).unwrap();

        assert_eq!(absent.reply_to, None);
        assert_eq!(cleared.reply_to, Some(None));
        assert_eq!(set.reply_to, Some(Some("a@example.com".into())));
    }

    #[test]
    fn each_identity_refusal_maps_to_its_status() {
        assert_eq!(
            identity_error(IdentityError::NotFound).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            identity_error(IdentityError::OffDomain).status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            identity_error(IdentityError::Taken).status(),
            StatusCode::CONFLICT
        );
        assert_eq!(archived().code(), Some("sender_identity_archived"));
    }

    #[test]
    fn a_body_marks_an_archived_identity() {
        let identity = SenderIdentity {
            id: Uuid::nil(),
            domain_id: Uuid::nil(),
            from_name: "N".into(),
            from_address: "n@example.com".into(),
            reply_to: None,
            archived_at: Some(OffsetDateTime::UNIX_EPOCH),
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };

        let json = serde_json::to_value(SenderIdentityBody::from(&identity)).unwrap();

        assert_eq!(json["archived"], true);
        assert!(json.get("archived_at").is_none());
    }
}
