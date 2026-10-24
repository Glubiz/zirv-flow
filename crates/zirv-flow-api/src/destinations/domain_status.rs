//! What a provider destination says about a sending domain: the per-record status its manifest's
//! `domain_status` request returns, shown next to our own record status (#217). It is a read. It
//! asks the provider and answers; it never marks a domain verified, never changes a record and
//! writes nothing, so the verification of a domain stays the DNS check's alone.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_connectors::destinations::{
    DestinationContext, DestinationError, ProviderRecord, provider_domain_status,
};
use zirv_flow_connectors::http::HttpLimits;
use zirv_flow_core::sending::{RecordKind, record_host};
use zirv_flow_store::{DomainRecord, SendingDomain};

use super::actions::{CALL_TIMEOUT, describe};
use super::{archived, kind_unavailable, registry};
use crate::AppState;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::problem::{ApiError, Problem};
use crate::workspace_credentials::resolve;

/// The `domain_id` of the query string: a UUID needs no decoding, so anything else is no domain.
fn domain_id_of(query: Option<&str>) -> Result<Uuid, ApiError> {
    query
        .into_iter()
        .flat_map(|query| query.split('&'))
        .find_map(|pair| pair.strip_prefix("domain_id="))
        .and_then(|value| Uuid::parse_str(value).ok())
        .ok_or_else(|| {
            ApiError::bad_request("the domain_id query parameter must be a sending domain id")
        })
}

/// What the provider reports for one record.
#[derive(Serialize, ToSchema)]
pub(crate) struct ProviderRecordBody {
    /// The record's DNS type as the provider names it, such as `TXT`.
    #[schema(example = "TXT")]
    record_type: String,
    /// `verified`, `pending` or `failed`, as the manifest maps the provider's own value; `unknown`
    /// when it maps none.
    #[schema(example = "verified")]
    status: String,
}

/// One record of the domain: our status and the provider's.
#[derive(Serialize, ToSchema)]
pub(crate) struct DomainStatusRecordBody {
    /// `dkim`, `spf` or `dmarc`.
    kind: String,
    /// The DNS name of the record.
    host: String,
    /// Our status of the record, from the DNS check: `pending`, `ok`, `mismatch` or `missing`.
    status: String,
    /// The provider's view of the record at the same name; null when it lists none there.
    provider: Option<ProviderRecordBody>,
}

/// A record the provider lists that is none of ours (a return-path or tracking record).
#[derive(Serialize, ToSchema)]
pub(crate) struct ProviderOnlyRecordBody {
    name: String,
    record_type: String,
    status: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct DomainStatusBody {
    domain_id: Uuid,
    domain: String,
    /// The domain's own status, `verified` or `unverified`. Only the DNS check changes it.
    status: String,
    records: Vec<DomainStatusRecordBody>,
    provider_only: Vec<ProviderOnlyRecordBody>,
}

fn not_declared() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "this destination declares no domain-status request",
    )
    .with_code("domain_status_unsupported")
}

/// A DNS name compared the way DNS does: ignoring case and a trailing dot.
fn same_name(a: &str, b: &str) -> bool {
    a.trim_end_matches('.')
        .eq_ignore_ascii_case(b.trim_end_matches('.'))
}

/// Lays the provider's records next to ours, by DNS name.
fn combine(
    domain: &SendingDomain,
    ours: &[DomainRecord],
    theirs: &[ProviderRecord],
) -> DomainStatusBody {
    let mut used = vec![false; theirs.len()];
    let records = ours
        .iter()
        .filter_map(|record| {
            let kind = RecordKind::parse(&record.kind)?;
            let host = record_host(kind, &domain.selector, &domain.domain);
            let found = theirs
                .iter()
                .enumerate()
                .find(|(_, theirs)| same_name(&theirs.name, &host));
            if let Some((position, _)) = found {
                used[position] = true;
            }
            Some(DomainStatusRecordBody {
                kind: record.kind.clone(),
                host,
                status: record.status.clone(),
                provider: found.map(|(_, theirs)| ProviderRecordBody {
                    record_type: theirs.record_type.clone(),
                    status: theirs.status.as_str().to_owned(),
                }),
            })
        })
        .collect();
    let provider_only = theirs
        .iter()
        .zip(&used)
        .filter(|(_, used)| !**used)
        .map(|(theirs, _)| ProviderOnlyRecordBody {
            name: theirs.name.clone(),
            record_type: theirs.record_type.clone(),
            status: theirs.status.as_str().to_owned(),
        })
        .collect();
    DomainStatusBody {
        domain_id: domain.id,
        domain: domain.domain.clone(),
        status: domain.status.clone(),
        records,
        provider_only,
    }
}

/// Asks a provider destination for the state of one sending domain and shows it next to ours.
/// It needs the Admin role, as the other destination endpoints do: it calls the provider with the
/// stored credential, and a redirect from the provider is an upstream error, never followed. It is
/// read-only: the provider's answer is shown and never
/// promotes the domain, whose status only the DNS check sets. A destination whose manifest
/// declares no domain-status request is a 404 with code `domain_status_unsupported`.
#[utoipa::path(
    get,
    operation_id = "get_destination_domain_status",
    path = "/api/v1/workspaces/{ws}/destinations/{id}/domain-status",
    tag = "destinations",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Destination id"),
        ("domain_id" = Uuid, Query, description = "The sending domain to ask the provider about")
    ),
    responses(
        (status = 200, description = "Our record status and the provider's, side by side", body = DomainStatusBody),
        (status = 400, description = "No valid domain_id query parameter", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or a missing scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, destination or sending domain, no access, or the destination declares no domain-status request (code `domain_status_unsupported`)", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The destination is archived (`destination_archived`), or its type is not in this build (`destination_kind_unavailable`)", body = Problem, content_type = "application/problem+json"),
        (status = 502, description = "The provider failed or answered with an error; `code` says which", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key to open the credential", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "The provider did not answer in time", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Json<DomainStatusBody>, ApiError> {
    let domain_id = domain_id_of(query.as_deref())?;
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let destination = zirv_flow_store::find_destination(&state.pool, workspace.id, path_id(&id)?)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if destination.archived_at.is_some() {
        return Err(archived());
    }
    let driver = registry()
        .get(&destination.kind)
        .ok_or_else(kind_unavailable)?;
    if driver.spec().kind != "provider" {
        return Err(not_declared());
    }
    let domain = zirv_flow_store::find_sending_domain(&state.pool, workspace.id, domain_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let credential = resolve::load(&state, workspace.id, destination.credential_id).await?;
    let network = DestinationContext::new(
        state.egress_policy().clone(),
        HttpLimits {
            max_redirects: 0,
            ..HttpLimits::default()
        },
        driver.spec().kind,
        destination.id.to_string(),
    )
    .map_err(|_| ApiError::internal())?;
    let reply = tokio::time::timeout(
        CALL_TIMEOUT,
        provider_domain_status(&network, &destination.config, &credential, &domain.domain),
    )
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::GATEWAY_TIMEOUT,
            "the provider did not answer in time",
        )
    })?;
    let theirs = match reply {
        Ok(records) => records,
        Err(DestinationError::Unsupported) => return Err(not_declared()),
        Err(error) => {
            let (code, message) = describe(&error);
            return Err(ApiError::new(StatusCode::BAD_GATEWAY, message).with_code(code));
        }
    };
    let ours = zirv_flow_store::list_domain_records(&state.pool, workspace.id, domain.id).await?;
    Ok(Json(combine(&domain, &ours, &theirs)))
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;
    use zirv_flow_connectors::destinations::ProviderRecordStatus;

    use super::*;

    fn domain() -> SendingDomain {
        SendingDomain {
            id: Uuid::from_u128(1),
            domain: "news.example.test".to_owned(),
            selector: "zirv".to_owned(),
            dkim_secret_id: Uuid::from_u128(2),
            dkim_public_key: String::new(),
            status: "unverified".to_owned(),
            verified_at: None,
            last_checked_at: None,
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn record(kind: &str, status: &str) -> DomainRecord {
        DomainRecord {
            kind: kind.to_owned(),
            expected_text: "v=x".to_owned(),
            status: status.to_owned(),
            observed_text: Vec::new(),
            checked_at: None,
        }
    }

    fn theirs(name: &str, status: ProviderRecordStatus) -> ProviderRecord {
        ProviderRecord {
            name: name.to_owned(),
            record_type: "TXT".to_owned(),
            status,
        }
    }

    #[test]
    fn the_providers_records_sit_next_to_ours_by_name_and_the_rest_are_listed_apart() {
        let ours = [record("dkim", "ok"), record("spf", "missing")];
        let reply = [
            theirs(
                "ZIRV._domainkey.news.example.test.",
                ProviderRecordStatus::Verified,
            ),
            theirs("bounces.news.example.test", ProviderRecordStatus::Pending),
        ];

        let body = combine(&domain(), &ours, &reply);

        assert_eq!(
            body.status, "unverified",
            "the domain's status is not touched"
        );
        assert_eq!(body.records.len(), 2);
        assert_eq!(body.records[0].status, "ok");
        assert_eq!(
            body.records[0].provider.as_ref().unwrap().status,
            "verified"
        );
        assert!(body.records[1].provider.is_none());
        assert_eq!(body.provider_only.len(), 1);
        assert_eq!(body.provider_only[0].name, "bounces.news.example.test");
    }

    #[test]
    fn the_domain_id_is_read_from_the_query_and_anything_else_is_a_bad_request() {
        let id = Uuid::from_u128(7);

        assert_eq!(domain_id_of(Some(&format!("domain_id={id}"))).unwrap(), id);
        assert_eq!(
            domain_id_of(Some(&format!("x=1&domain_id={id}&y=2"))).unwrap(),
            id
        );
        for bad in [
            None,
            Some(""),
            Some("domain_id="),
            Some("domain_id=nope"),
            Some("id=1"),
        ] {
            assert_eq!(
                domain_id_of(bad).unwrap_err().status_code(),
                StatusCode::BAD_REQUEST
            );
        }
    }

    #[test]
    fn names_compare_ignoring_case_and_a_trailing_dot() {
        assert!(same_name("A.Example.TEST.", "a.example.test"));
        assert!(!same_name("a.example.test", "b.example.test"));
    }
}
