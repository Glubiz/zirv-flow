//! The sending domains of a workspace: create (which generates and seals the DKIM key), read with
//! the status of each record, ask for a re-check, and delete. Everything here is for workspace
//! admins, and every change is audited with ids and names, never a key.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::sending::{
    DEFAULT_SELECTOR, RecordKind, generate_dkim_key, parse_domain, parse_selector,
    parse_spf_mechanism, record_host, redact_observed,
};
use zirv_flow_core::{SecretPurpose, TraceContext, UserId, WorkspaceId};
use zirv_flow_store::{
    DomainDelete, DomainRecord, NewSendingDomain, PgPool, SendingDomain, secrets,
};

use super::invalid_field;
use crate::AppState;
use crate::admin::{self, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};
use crate::sources::credential::keyring;
use crate::trace::request_trace;

/// One DNS record a domain must publish, and what the last check found.
#[derive(Serialize, ToSchema)]
pub(crate) struct DomainRecordBody {
    /// `dkim`, `spf` or `dmarc`.
    #[schema(example = "dkim")]
    kind: String,
    /// The DNS name to publish the TXT record at.
    #[schema(example = "zirv._domainkey.example.com")]
    host: String,
    /// The TXT value to publish. The SPF value is a recommendation: a name holds one `v=spf1`
    /// record, so merge it with the one the domain already has.
    expected_text: String,
    /// `pending` (not checked yet), `ok`, `mismatch` (a record is there but does not satisfy the
    /// rule) or `missing`.
    status: String,
    /// The TXT records the last check read at `host`, bounded in number and length.
    observed_text: Vec<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    checked_at: Option<OffsetDateTime>,
}

/// A sending domain. It never carries the private key or the id of its secret.
#[derive(Serialize, ToSchema)]
pub(crate) struct SendingDomainBody {
    id: Uuid,
    #[schema(example = "example.com")]
    domain: String,
    /// The DKIM selector: the record is published at `<selector>._domainkey.<domain>`.
    selector: String,
    /// `verified` only while the DKIM, SPF and DMARC records are all `ok`; otherwise
    /// `unverified`, and no email template naming a sender on it can be published.
    status: String,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    verified_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    last_checked_at: Option<OffsetDateTime>,
    /// The three records, in the order DKIM, SPF, DMARC.
    records: Vec<DomainRecordBody>,
    /// The user who created it; null when an API key did.
    created_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
}

impl SendingDomainBody {
    fn new(domain: &SendingDomain, records: &[DomainRecord]) -> Self {
        Self {
            id: domain.id,
            domain: domain.domain.clone(),
            selector: domain.selector.clone(),
            status: domain.status.clone(),
            verified_at: domain.verified_at,
            last_checked_at: domain.last_checked_at,
            records: records
                .iter()
                .filter_map(|record| {
                    let kind = RecordKind::parse(&record.kind)?;
                    Some(DomainRecordBody {
                        kind: record.kind.clone(),
                        host: record_host(kind, &domain.selector, &domain.domain),
                        expected_text: record.expected_text.clone(),
                        status: record.status.clone(),
                        observed_text: redact_observed(kind, &record.observed_text),
                        checked_at: record.checked_at,
                    })
                })
                .collect(),
            created_by: domain.created_by,
            created_at: domain.created_at,
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateSendingDomainRequest {
    /// The domain to send from, such as `example.com`: letters, digits and hyphens in at least two
    /// labels. Give an internationalized name in punycode.
    #[schema(example = "example.com")]
    domain: String,
    /// The DKIM selector, 1 to 63 letters, digits or hyphens. Default `zirv`.
    selector: Option<String>,
    /// One SPF mechanism the domain's SPF record must contain, such as `include:spf.relay.example`,
    /// `ip4:203.0.113.7`, `a` or `mx`. Without one, any `v=spf1` record satisfies the check.
    spf_mechanism: Option<String>,
}

fn domain_exists() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "the workspace already has that sending domain",
    )
}

async fn records_of(
    pool: &PgPool,
    workspace: WorkspaceId,
    domain: &SendingDomain,
) -> Result<SendingDomainBody, ApiError> {
    let records = zirv_flow_store::list_domain_records(pool, workspace, domain.id).await?;
    Ok(SendingDomainBody::new(domain, &records))
}

/// Registers a domain: generates its RSA-2048 DKIM key, seals the private half, stores the
/// domain with its three pending records, queues the first DNS check and audits
/// `sending_domain.created` (and `secret.created` for the key), all in one transaction.
pub(crate) async fn create_sending_domain(
    state: &AppState,
    context: &Context,
    workspace: WorkspaceId,
    created_by: Option<UserId>,
    trace: TraceContext,
    request: &CreateSendingDomainRequest,
) -> Result<(SendingDomain, Vec<DomainRecord>), ApiError> {
    let domain = parse_domain(&request.domain).map_err(invalid_field)?;
    let selector = match &request.selector {
        Some(selector) => parse_selector(selector).map_err(invalid_field)?,
        None => DEFAULT_SELECTOR.to_owned(),
    };
    let mechanism = request
        .spf_mechanism
        .as_deref()
        .map(parse_spf_mechanism)
        .transpose()
        .map_err(invalid_field)?;
    // Without a master key the key could not be sealed; say so before spending the CPU.
    let keyring = keyring(state)?;
    let key = tokio::task::spawn_blocking(generate_dkim_key)
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(|_| ApiError::internal())?;

    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let secret = admin::create_secret_in(
        &mut tx,
        context,
        keyring,
        workspace,
        SecretPurpose::DkimKey,
        &key.private_pkcs8,
    )
    .await?;
    let (created, records) = zirv_flow_store::create_sending_domain(
        &mut tx,
        workspace,
        &NewSendingDomain {
            id: Uuid::now_v7(),
            domain: &domain,
            selector: &selector,
            dkim_secret_id: secret,
            dkim_public_key: &key.public_key,
            spf_mechanism: mechanism.as_deref(),
            created_by,
        },
    )
    .await
    .map_err(|error| {
        if error.is_conflict() {
            domain_exists()
        } else {
            error.into()
        }
    })?;
    zirv_flow_store::enqueue_domain_verify(&mut tx, workspace, created.id, trace).await?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "sending_domain.created",
        Some(("sending_domain", created.id)),
        json!({"domain": created.domain, "selector": created.selector}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((created, records))
}

/// Queues a DNS check of the domain now, unless one is already waiting, and audits
/// `sending_domain.verification_requested`.
pub(crate) async fn request_verification(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
    trace: TraceContext,
) -> Result<(SendingDomain, Vec<DomainRecord>), ApiError> {
    let mut tx = zirv_flow_store::begin(pool).await?;
    let domain = zirv_flow_store::find_sending_domain(&mut *tx, workspace, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let queued = zirv_flow_store::enqueue_domain_verify(&mut tx, workspace, id, trace).await?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "sending_domain.verification_requested",
        Some(("sending_domain", id)),
        json!({"domain": domain.domain, "queued": queued}),
    )
    .await?;
    let records = zirv_flow_store::list_domain_records(&mut *tx, workspace, id).await?;
    zirv_flow_store::commit(tx).await?;
    Ok((domain, records))
}

/// Deletes a domain nothing uses, with its records, its archived identities and its sealed key.
/// Audited as `sending_domain.deleted` and `secret.deleted`.
pub(crate) async fn delete_sending_domain(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<(), ApiError> {
    let mut tx = zirv_flow_store::begin(pool).await?;
    let domain = zirv_flow_store::find_sending_domain(&mut *tx, workspace, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    match zirv_flow_store::delete_sending_domain(&mut tx, workspace, id).await? {
        DomainDelete::NotFound => return Err(ApiError::not_found()),
        DomainDelete::InUse => return Err(ApiError::sending_domain_in_use()),
        DomainDelete::Deleted { secret } => {
            secrets::delete(&mut *tx, workspace, secret).await?;
            admin::audit_secret_deleted(&mut tx, context, workspace, secret).await?;
        }
    }
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "sending_domain.deleted",
        Some(("sending_domain", id)),
        json!({"domain": domain.domain}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(())
}

/// The workspace's sending domains, each with its records. Workspace admins only.
#[utoipa::path(
    get,
    operation_id = "list_sending_domains",
    path = "/api/v1/workspaces/{ws}/sending-domains",
    tag = "sending",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The sending domains by name", body = Vec<SendingDomainBody>),
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
) -> Result<Json<Vec<SendingDomainBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let domains = zirv_flow_store::list_sending_domains(&state.pool, workspace.id).await?;
    let all_records =
        zirv_flow_store::list_workspace_domain_records(&state.pool, workspace.id).await?;
    let bodies = domains
        .iter()
        .map(|domain| {
            let records: Vec<DomainRecord> = all_records
                .iter()
                .filter(|(id, _)| *id == domain.id)
                .map(|(_, record)| record.clone())
                .collect();
            SendingDomainBody::new(domain, &records)
        })
        .collect();
    Ok(Json(bodies))
}

/// Registers a sending domain and returns the DNS records to publish. Zirv Flow generates the
/// DKIM key; only its public half appears, in the DKIM record text. A DNS check is queued at once.
/// Workspace admins only. Audited as `sending_domain.created`.
#[utoipa::path(
    post,
    operation_id = "create_sending_domain",
    path = "/api/v1/workspaces/{ws}/sending-domains",
    tag = "sending",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = CreateSendingDomainRequest,
    responses(
        (status = 201, description = "The new domain with its records to publish", body = SendingDomainBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The workspace already has the domain, or the master key is outdated", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid domain, selector or SPF mechanism", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key to seal the DKIM key", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<CreateSendingDomainRequest>,
) -> Result<(StatusCode, Json<SendingDomainBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let (domain, records) = create_sending_domain(
        &state,
        &principal.context(),
        workspace.id,
        principal.user_id(),
        request_trace(),
        &body,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(SendingDomainBody::new(&domain, &records)),
    ))
}

/// One sending domain with the status of each record. Workspace admins only.
#[utoipa::path(
    get,
    operation_id = "get_sending_domain",
    path = "/api/v1/workspaces/{ws}/sending-domains/{id}",
    tag = "sending",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Sending domain id")
    ),
    responses(
        (status = 200, description = "The domain and its records", body = SendingDomainBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or a missing scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or domain, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<SendingDomainBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let domain = zirv_flow_store::find_sending_domain(&state.pool, workspace.id, path_id(&id)?)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(records_of(&state.pool, workspace.id, &domain).await?))
}

/// Asks for a DNS check of the domain now. The check runs in the background, so the response
/// shows the state before it; read the domain again for the result. A check already waiting
/// serves this request too. Workspace admins only. Audited as
/// `sending_domain.verification_requested`.
#[utoipa::path(
    post,
    operation_id = "verify_sending_domain",
    path = "/api/v1/workspaces/{ws}/sending-domains/{id}/verify",
    tag = "sending",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Sending domain id")
    ),
    responses(
        (status = 202, description = "The check is queued; the domain as it is now", body = SendingDomainBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or domain, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn verify(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<(StatusCode, Json<SendingDomainBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let (domain, records) = request_verification(
        &state.pool,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
        request_trace(),
    )
    .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(SendingDomainBody::new(&domain, &records)),
    ))
}

/// Deletes a sending domain, its records, its archived sender identities and its DKIM key. It is
/// refused while a live sender identity exists on it, or a template names one of its identities.
/// Workspace admins only. Audited as `sending_domain.deleted`.
#[utoipa::path(
    delete,
    operation_id = "delete_sending_domain",
    path = "/api/v1/workspaces/{ws}/sending-domains/{id}",
    tag = "sending",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Sending domain id")
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or domain, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The domain still has sender identities in use (`code` is `sending_domain_in_use`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn remove(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    delete_sending_domain(
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

    fn domain() -> SendingDomain {
        SendingDomain {
            id: Uuid::nil(),
            domain: "example.com".into(),
            selector: "zirv".into(),
            dkim_secret_id: Uuid::nil(),
            dkim_public_key: "AAAA".into(),
            status: "unverified".into(),
            verified_at: None,
            last_checked_at: None,
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn record(kind: &str) -> DomainRecord {
        DomainRecord {
            kind: kind.into(),
            expected_text: format!("text {kind}"),
            status: "pending".into(),
            observed_text: Vec::new(),
            checked_at: None,
        }
    }

    #[test]
    fn a_body_names_the_host_of_each_record_and_carries_no_secret_reference() {
        let body = SendingDomainBody::new(
            &domain(),
            &[
                record("dkim"),
                record("spf"),
                record("dmarc"),
                record("other"),
            ],
        );

        let json = serde_json::to_value(&body).unwrap();

        let hosts: Vec<_> = json["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|record| record["host"].as_str().unwrap())
            .collect();
        assert_eq!(
            hosts,
            [
                "zirv._domainkey.example.com",
                "example.com",
                "_dmarc.example.com"
            ]
        );
        assert!(json.get("dkim_secret_id").is_none());
        assert!(json.get("dkim_public_key").is_none());
        assert!(!json.to_string().contains("secret"));
    }

    #[test]
    fn a_request_names_a_domain_and_optionally_a_selector_and_an_spf_mechanism() {
        let request: CreateSendingDomainRequest =
            serde_json::from_str(r#"{"domain":"example.com"}"#).unwrap();

        assert_eq!(request.domain, "example.com");
        assert!(request.selector.is_none() && request.spf_mechanism.is_none());
    }
}
