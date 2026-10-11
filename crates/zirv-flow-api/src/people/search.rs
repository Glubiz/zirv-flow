//! People search (architecture section 12.3): by identifier or by one trait.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_store::{PersonSearch, SearchError, SearchHit, TraitOp};

use crate::AppState;
use crate::auth::{Need, Principal, authorize, may_see_values, path_id};
use crate::jobs_admin::{DEFAULT_LIMIT, MAX_LIMIT};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

/// An identifier to find: the value is normalized for the kind, then matched exactly.
#[derive(Deserialize, ToSchema)]
pub(crate) struct IdentifierQuery {
    /// `email`, `phone`, `anonymous_id` or `external:<namespace>`.
    #[schema(example = "email")]
    kind: String,
    value: String,
}

/// A trait to compare. The operators that apply depend on the trait's registry type: `eq`
/// and `exists` for any type (a `string_list` matches when it holds the value), `prefix` for
/// strings, and `gt`, `gte`, `lt` and `lte` for numbers, timestamps and dates.
#[derive(Deserialize, ToSchema)]
pub(crate) struct TraitQuery {
    #[schema(example = "plan")]
    key: String,
    #[schema(example = "eq")]
    op: String,
    /// Required for every operator but `exists`; it must fit the trait's type.
    #[serde(default)]
    value: Option<Value>,
}

/// Exactly one of `identifier` and `trait`. It is a POST so the values stay out of URLs and
/// access logs.
#[derive(Deserialize, ToSchema)]
pub(crate) struct SearchRequest {
    identifier: Option<IdentifierQuery>,
    #[serde(rename = "trait")]
    #[schema(rename = "trait")]
    trait_query: Option<TraitQuery>,
    /// People per page, 1 to 200; default 50.
    limit: Option<u32>,
    /// The `next_after` of the previous page.
    after: Option<Uuid>,
}

/// An identifier shown for a person. The value is null when `masked`: only workspace admins,
/// the owner and `admin` keys see identifier values.
#[derive(Serialize, ToSchema)]
pub(crate) struct DisplayBody {
    #[schema(example = "email")]
    pub(crate) kind: String,
    pub(crate) value: Option<String>,
    pub(crate) masked: bool,
}

impl DisplayBody {
    pub(crate) fn new(kind: String, value: String, reveal: bool) -> Self {
        Self {
            kind,
            value: reveal.then_some(value),
            masked: !reveal,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct PersonHitBody {
    /// The surviving person: a merged person is never listed.
    id: Uuid,
    /// The strongest identifier; null for a person with none.
    display: Option<DisplayBody>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    updated_at: OffsetDateTime,
}

impl PersonHitBody {
    fn new(hit: SearchHit, reveal: bool) -> Self {
        Self {
            id: hit.id,
            display: hit
                .display
                .map(|(kind, value)| DisplayBody::new(kind, value, reveal)),
            updated_at: hit.updated_at,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SearchBody {
    /// By person id.
    people: Vec<PersonHitBody>,
    /// Pass as `after` for the next page; null when there is none.
    next_after: Option<Uuid>,
}

/// Turns the request into a store search, refusing a body that does not name exactly one.
fn parse(body: SearchRequest) -> Result<(PersonSearch, u32, Option<Uuid>), ApiError> {
    let limit = match body.limit {
        None => DEFAULT_LIMIT,
        Some(limit) if (1..=MAX_LIMIT).contains(&limit) => limit,
        Some(_) => {
            return Err(ApiError::unprocessable(format!(
                "limit: must be from 1 to {MAX_LIMIT}"
            )));
        }
    };
    let search = match (body.identifier, body.trait_query) {
        (Some(identifier), None) => PersonSearch::Identifier {
            kind: identifier.kind,
            value: identifier.value,
        },
        (None, Some(query)) => PersonSearch::Trait {
            key: query.key,
            op: query
                .op
                .parse::<TraitOp>()
                .map_err(|()| ApiError::unprocessable("trait.op: is not a known operator"))?,
            value: query.value,
        },
        _ => {
            return Err(ApiError::unprocessable(
                "search: give exactly one of identifier and trait",
            ));
        }
    };
    Ok((search, limit, body.after))
}

/// Finds people by an identifier, matched exactly after normalization and following merges to
/// the surviving person, or by one trait with an operator that fits its type. Results are
/// paged by person id; identifier values are shown to the owner, workspace admins and
/// `admin` keys and masked for everyone else. A search that runs past its time limit is
/// answered 503 with the code `search_timeout`.
#[utoipa::path(
    post,
    operation_id = "search_people",
    path = "/api/v1/workspaces/{ws}/people/search",
    tag = "people",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = SearchRequest,
    responses(
        (status = 200, description = "A page of people", body = SearchBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Not exactly one of identifier and trait, an invalid identifier, an unknown trait or operator, an operator that does not fit the trait's type, or a value that does not fit it", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The search ran past its time limit (`code` is `search_timeout`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn search(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<SearchRequest>,
) -> Result<Json<SearchBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let (search, limit, after) = parse(body)?;
    let page = zirv_flow_store::search_people(&state.pool, workspace.id, &search, limit, after)
        .await
        .map_err(|error| match error {
            SearchError::Invalid { field, reason } => {
                ApiError::unprocessable(format!("{field}: {reason}"))
            }
            SearchError::Store(error) => error.into(),
        })?;
    let reveal = may_see_values(&state, &principal, workspace.id).await;
    Ok(Json(SearchBody {
        people: page
            .hits
            .into_iter()
            .map(|hit| PersonHitBody::new(hit, reveal))
            .collect(),
        next_after: page.next_after,
    }))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;

    use super::*;

    fn request(body: Value) -> SearchRequest {
        serde_json::from_value(body).unwrap()
    }

    #[test]
    fn a_search_names_exactly_one_of_identifier_and_trait() {
        let identifier = json!({"kind": "email", "value": "a@example.com"});
        let by_trait = json!({"key": "plan", "op": "eq", "value": "pro"});

        let ok = parse(request(json!({"identifier": identifier}))).unwrap();
        let both = parse(request(
            json!({"identifier": identifier, "trait": by_trait}),
        ));
        let neither = parse(request(json!({})));

        assert!(matches!(ok.0, PersonSearch::Identifier { .. }));
        assert_eq!((ok.1, ok.2), (50, None));
        for refused in [both, neither] {
            assert_eq!(
                refused.unwrap_err().status(),
                StatusCode::UNPROCESSABLE_ENTITY
            );
        }
    }

    #[test]
    fn a_trait_search_takes_a_known_operator_and_an_optional_value() {
        let exists = parse(request(json!({"trait": {"key": "plan", "op": "exists"}}))).unwrap();
        assert!(matches!(
            exists.0,
            PersonSearch::Trait {
                op: TraitOp::Exists,
                value: None,
                ..
            }
        ));
        let bad = parse(request(
            json!({"trait": {"key": "plan", "op": "like", "value": "x"}}),
        ))
        .unwrap_err();
        assert_eq!(bad.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(bad.detail(), "trait.op: is not a known operator");
    }

    #[test]
    fn the_limit_is_one_to_two_hundred() {
        let with = |limit| {
            parse(request(json!({
                "identifier": {"kind": "email", "value": "a@example.com"},
                "limit": limit
            })))
        };

        assert_eq!(with(200).unwrap().1, 200);
        assert_eq!(with(1).unwrap().1, 1);
        for bad in [0, 201] {
            assert_eq!(
                with(bad).unwrap_err().status(),
                StatusCode::UNPROCESSABLE_ENTITY
            );
        }
    }

    #[test]
    fn a_masked_identifier_shows_its_kind_and_no_value() {
        let shown = DisplayBody::new("email".into(), "a@example.com".into(), true);
        let hidden = DisplayBody::new("email".into(), "a@example.com".into(), false);

        assert_eq!(shown.value.as_deref(), Some("a@example.com"));
        assert!(!shown.masked);
        assert_eq!(
            (hidden.kind.as_str(), hidden.value, hidden.masked),
            ("email", None, true)
        );
    }
}
