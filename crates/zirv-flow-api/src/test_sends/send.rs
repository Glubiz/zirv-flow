//! Starting a test send and reading the log.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_connectors::destinations::Registry;
use zirv_flow_core::UserId;
use zirv_flow_core::people::{Identifier, identifier_hash};
use zirv_flow_store::{NewTestSend, TestSendReservation};

use super::TestSendBody;
use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::jobs_admin::{encode_cursor, parse_page};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};
use crate::trace;

/// Who a test goes to. Exactly one of the two fields is given.
#[derive(Deserialize, ToSchema)]
pub(crate) struct TestSendRecipient {
    /// A member of this workspace; the test goes to the address on their account.
    member: Option<Uuid>,
    /// An address on the workspace's test-send allowlist.
    #[schema(example = "qa@example.com")]
    address: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct TestSendRequest {
    /// The published version to send. Absent: the current draft.
    version: Option<i32>,
    /// The destination to send through, as a send node names it.
    destination_id: Uuid,
    /// The sender identity, as a send node names it. It must be live on a verified domain, or the
    /// test ends `sender_unverified`.
    sender_identity_id: Uuid,
    to: TestSendRecipient,
    /// Render with this person's values; needs the workspace admin role, as a preview does.
    /// Absent: the sample person. The person is not recorded in the log.
    person_id: Option<Uuid>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct TestSendsBody {
    /// Newest first.
    test_sends: Vec<TestSendBody>,
    /// Pass as `before` for the next page; null when there is none.
    next_before: Option<String>,
}

fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "test sends are unavailable: the server has no master key",
    )
}

/// The problem for a recipient a test may not go to.
fn recipient_not_allowed() -> ApiError {
    ApiError::unprocessable(
        "a test goes only to a member of this workspace or to an address on the allowlist",
    )
    .with_code("recipient_not_allowed")
}

/// Sends a template to one workspace member or allowlisted address through a real destination,
/// marked `[TEST]`, with no tracking and no trace in the message log. The reply is the log row,
/// `queued`; poll the log for the outcome. At most 10 tests per member and 100 per workspace start in
/// any rolling hour. Needs the write role, or the admin role with `person_id`, and the CSRF token.
/// Audited as `template.test_sent`, with ids only.
#[utoipa::path(
    post,
    operation_id = "test_send_template",
    path = "/api/v1/workspaces/{ws}/templates/{id}/test-send",
    tag = "templates",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Template id")
    ),
    request_body = TestSendRequest,
    responses(
        (status = 202, description = "The log row, queued", body = TestSendBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, a missing CSRF token, or a person asked for without the admin role (`code` is `template_preview_values_forbidden`)", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, template, version, destination or person, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The template is archived (`template_archived`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The recipient is neither a member nor allowlisted (`recipient_not_allowed`), the template or destination does not do email, or the body is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "The member or the workspace reached its hourly limit; `Retry-After` is the seconds until a slot frees (`test_send_rate_limited`)", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn test_send(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<TestSendRequest>,
) -> Result<(StatusCode, Json<TestSendBody>), ApiError> {
    let ws = path_id(&ws)?;
    let workspace = if body.person_id.is_some() {
        // Until the `view_pii` grant exists, a real person's values are for admins.
        authorize(&state, &principal, ws, Need::Admin)
            .await
            .map_err(|error| {
                if error.status_code() == StatusCode::FORBIDDEN {
                    ApiError::template_preview_values_forbidden()
                } else {
                    error
                }
            })?
    } else {
        authorize(&state, &principal, ws, Need::Write).await?
    };
    let id: Uuid = path_id(&id)?;
    let key = state.identifier_key.as_deref().ok_or_else(unavailable)?;
    let trace = trace::current().ok_or_else(ApiError::internal)?;

    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let template = zirv_flow_store::find_template(&mut *tx, workspace.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if template.archived_at.is_some() {
        return Err(ApiError::template_archived());
    }
    if template.channel != "email" {
        return Err(ApiError::unprocessable(
            "only an email template can be test-sent",
        ));
    }
    match body.version {
        Some(number) => {
            zirv_flow_store::find_template_version(&mut *tx, workspace.id, id, number)
                .await?
                .ok_or_else(ApiError::not_found)?;
        }
        None => {
            zirv_flow_store::find_template_draft(&mut *tx, workspace.id, id)
                .await?
                .ok_or_else(ApiError::not_found)?;
        }
    }
    let destination =
        zirv_flow_store::find_destination(&mut *tx, workspace.id, body.destination_id)
            .await?
            .filter(|destination| destination.archived_at.is_none())
            .ok_or_else(ApiError::not_found)?;
    let does_email = Registry::built_in()
        .get(&destination.kind)
        .is_some_and(|driver| driver.spec().capabilities.channels.contains(&"email"));
    if !does_email {
        return Err(ApiError::unprocessable(
            "the destination does not send email",
        ));
    }
    if let Some(person) = body.person_id {
        zirv_flow_store::find_person(&mut *tx, workspace.id, person)
            .await?
            .ok_or_else(ApiError::not_found)?;
    }

    // The recipient: a member's account address, or an allowlisted address. A refusal is the same
    // problem whichever way it is reached, so it does not say which addresses are on the list.
    let (address, member) = match (body.to.member, body.to.address.as_deref()) {
        (Some(member), None) => {
            let user = UserId::from_uuid(member);
            zirv_flow_store::membership_role(&mut *tx, workspace.id, user)
                .await?
                .ok_or_else(recipient_not_allowed)?;
            let account = zirv_flow_store::find_user(&mut *tx, user)
                .await?
                .ok_or_else(recipient_not_allowed)?;
            (account.email, Some(member))
        }
        (None, Some(address)) => (address.to_owned(), None),
        _ => {
            return Err(ApiError::unprocessable(
                "`to` names exactly one of `member` and `address`",
            ));
        }
    };
    let identifier = Identifier::parse("email", &address).map_err(|_| recipient_not_allowed())?;
    let hash = identifier_hash(key, &identifier);
    if member.is_none() && !zirv_flow_store::is_allowlisted(&mut tx, workspace.id, &hash).await? {
        return Err(recipient_not_allowed());
    }

    let now = state.clock.now(&mut tx).await?;
    let send_id = Uuid::now_v7();
    let reservation = zirv_flow_store::reserve_test_send(
        &mut tx,
        &NewTestSend {
            id: send_id,
            workspace: workspace.id,
            template: id,
            template_version: body.version,
            member: principal.user_id().map(|user| user.as_uuid()),
            destination: destination.id,
            sender_identity: body.sender_identity_id,
            recipient_hash: hash,
            recipient_member: member,
            person: body.person_id,
            address: member.is_none().then_some(address.as_str()),
            now,
            trace: &trace,
        },
    )
    .await?;
    let row = match reservation {
        TestSendReservation::Reserved(row) => row,
        TestSendReservation::Limited { retry_after } => {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "too many test sends; try again later",
            )
            .with_code("test_send_rate_limited")
            .with_retry_after(retry_after));
        }
    };
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "template.test_sent",
        Some(("template", id)),
        json!({
            "test_send_id": send_id,
            "version": body.version,
            "destination_id": destination.id,
            "recipient": if member.is_some() { "member" } else { "allowlist" },
            "person": body.person_id.is_some(),
        }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((StatusCode::ACCEPTED, Json(row.into())))
}

/// The workspace's test-send log, newest first, optionally of one template (`template_id`).
#[utoipa::path(
    get,
    operation_id = "list_test_sends",
    path = "/api/v1/workspaces/{ws}/test-sends",
    tag = "templates",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("template_id" = Option<Uuid>, Query, description = "Only the tests of this template"),
        ("limit" = Option<u32>, Query, description = "Rows per page, 1 to 200; default 50"),
        ("before" = Option<String>, Query, description = "The next_before of the previous page")
    ),
    responses(
        (status = 200, description = "A page of the log", body = TestSendsBody),
        (status = 400, description = "limit or template_id is out of range or malformed, or before is not a cursor", body = Problem, content_type = "application/problem+json"),
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
) -> Result<Json<TestSendsBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let query = query.unwrap_or_default();
    let (limit, before) = parse_page(&query)?;
    let mut template = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if key == "template_id" {
            template = Some(
                value
                    .parse::<Uuid>()
                    .map_err(|_| ApiError::bad_request("template_id is not a valid id"))?,
            );
        }
    }
    let (rows, more) =
        zirv_flow_store::list_test_sends(&state.pool, workspace.id, template, limit, before)
            .await?;
    let next_before = match rows.last() {
        Some(last) if more => Some(encode_cursor((last.created_at, last.id))),
        _ => None,
    };
    Ok(Json(TestSendsBody {
        test_sends: rows.into_iter().map(TestSendBody::from).collect(),
        next_before,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_refusal_has_a_stable_code_and_does_not_name_the_address() {
        let error = recipient_not_allowed();

        assert_eq!(error.code(), Some("recipient_not_allowed"));
        assert_eq!(error.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn a_request_names_a_member_or_an_address() {
        let member: TestSendRequest = serde_json::from_value(json!({
            "destination_id": Uuid::nil(), "sender_identity_id": Uuid::nil(),
            "to": {"member": Uuid::nil()}
        }))
        .unwrap();
        let address: TestSendRequest = serde_json::from_value(json!({
            "version": 2, "destination_id": Uuid::nil(), "sender_identity_id": Uuid::nil(),
            "to": {"address": "qa@example.test"}, "person_id": Uuid::nil()
        }))
        .unwrap();

        assert!(member.to.member.is_some() && member.to.address.is_none());
        assert!(address.to.address.is_some() && address.version == Some(2));
    }
}
