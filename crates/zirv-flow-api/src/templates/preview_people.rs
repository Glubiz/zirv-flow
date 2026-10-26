//! Saved preview people (architecture 11.7): the people an admin keeps on a template to preview it
//! as. Every route is admin only, because it names real people, the same rule as a real-person
//! preview. Audits carry ids and never an identifier value.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_store::{PreviewPerson, PreviewPersonError};

use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::people::search::DisplayBody;
use crate::problem::{ApiError, Problem};

/// A person saved on a template.
#[derive(Serialize, ToSchema)]
pub(crate) struct PreviewPersonBody {
    /// The surviving person: a merged person is never listed.
    person_id: Uuid,
    /// The strongest identifier, as the people search shows it; null for a person with none.
    display: Option<DisplayBody>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
}

impl PreviewPersonBody {
    fn new(person: PreviewPerson, reveal: bool) -> Self {
        Self {
            person_id: person.person_id,
            display: person
                .display
                .map(|(kind, value)| DisplayBody::new(kind, value, reveal)),
            created_at: person.created_at,
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct AddPreviewPersonRequest {
    person_id: Uuid,
}

fn refused(error: PreviewPersonError) -> ApiError {
    match error {
        PreviewPersonError::NotFound => ApiError::not_found(),
        PreviewPersonError::Archived => ApiError::template_archived(),
        PreviewPersonError::Limit => ApiError::preview_people_limit(),
        PreviewPersonError::Store(error) => error.into(),
    }
}

/// The template's saved preview people, oldest first.
#[utoipa::path(
    get,
    operation_id = "list_template_preview_people",
    path = "/api/v1/workspaces/{ws}/templates/{id}/preview-people",
    tag = "templates",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Template id")
    ),
    responses(
        (status = 200, description = "The saved people", body = Vec<PreviewPersonBody>),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or a key without the admin scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or template, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn list(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<Vec<PreviewPersonBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let people = zirv_flow_store::list_preview_people(&mut tx, workspace.id, path_id(&id)?)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(
        people
            .into_iter()
            .map(|person| PreviewPersonBody::new(person, true))
            .collect(),
    ))
}

/// Saves a person on the template. A person already saved is answered 200 with the same row; a new
/// one is 201 and audited as `template.preview_person_added`.
#[utoipa::path(
    post,
    operation_id = "add_template_preview_person",
    path = "/api/v1/workspaces/{ws}/templates/{id}/preview-people",
    tag = "templates",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Template id")
    ),
    request_body = AddPreviewPersonRequest,
    responses(
        (status = 200, description = "The person was already saved", body = PreviewPersonBody),
        (status = 201, description = "The person was saved", body = PreviewPersonBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a key without the admin scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, template or person, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The template is archived (`template_archived`) or already keeps 20 people (`preview_people_limit`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn add(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<AddPreviewPersonRequest>,
) -> Result<(StatusCode, Json<PreviewPersonBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let id: Uuid = path_id(&id)?;
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let (person, created) = zirv_flow_store::add_preview_person(
        &mut tx,
        workspace.id,
        id,
        body.person_id,
        principal.user_id(),
    )
    .await
    .map_err(refused)?;
    if created {
        admin::audit(
            &mut tx,
            &principal.context(),
            Some(workspace.id),
            "template.preview_person_added",
            Some(("template", id)),
            json!({ "person_id": person.person_id }),
        )
        .await?;
    }
    zirv_flow_store::commit(tx).await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(PreviewPersonBody::new(person, true))))
}

/// Removes a saved person from the template. Audited as `template.preview_person_removed`.
#[utoipa::path(
    delete,
    operation_id = "remove_template_preview_person",
    path = "/api/v1/workspaces/{ws}/templates/{id}/preview-people/{person_id}",
    tag = "templates",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Template id"),
        ("person_id" = Uuid, Path, description = "Person id")
    ),
    responses(
        (status = 204, description = "The person was removed"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a key without the admin scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, template or saved person, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn remove(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id, person)): Path<(String, String, String)>,
) -> Result<StatusCode, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let (id, person): (Uuid, Uuid) = (path_id(&id)?, path_id(&person)?);
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    if !zirv_flow_store::remove_preview_person(&mut tx, workspace.id, id, person).await? {
        return Err(ApiError::not_found());
    }
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "template.preview_person_removed",
        Some(("template", id)),
        json!({ "person_id": person }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(StatusCode::NO_CONTENT)
}
