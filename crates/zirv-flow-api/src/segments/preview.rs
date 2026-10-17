//! Segment preview (architecture 8.3 and 7.5): the count and a sample of the people an unsaved
//! definition matches, and how old the data behind them is. It runs on the primary, in one
//! read-only snapshot under a 10 second statement timeout.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use utoipa::ToSchema;
use utoipa::openapi::{Ref, RefOr, Schema};
use uuid::Uuid;
use zirv_flow_segments::{Param, Query, TraitStorage, compile, parse_and_validate};
use zirv_flow_store::{DataAge, SqlParam, SqlQuery, StoreError};

use super::{catalog_of, issues_error};
use crate::AppState;
use crate::auth::{Need, Principal, authorize, may_see_values, path_id};
use crate::json::ApiJson;
use crate::people::search::DisplayBody;
use crate::problem::{ApiError, Problem};

const DEFAULT_SAMPLE: u32 = 20;
const MAX_SAMPLE: u32 = 100;

fn definition_schema() -> RefOr<Schema> {
    RefOr::Ref(Ref::from_schema_name(super::schema::DEFINITION))
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct PreviewRequest {
    /// The definition to preview, saved or not.
    #[schema(schema_with = definition_schema)]
    definition: Value,
    /// People to sample, 1 to 100; default 20. The sample is the first people by id.
    sample_size: Option<u32>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SampleBody {
    id: Uuid,
    /// The strongest identifier; null for a person with none. The value is masked for callers
    /// who may not see personal values.
    display: Option<DisplayBody>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct DataSourceBody {
    source_id: Uuid,
    name: String,
    /// When the source's oldest feeding stream last succeeded; null when one never has.
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    last_success_at: Option<OffsetDateTime>,
}

/// How current the data behind a preview is.
#[derive(Serialize, ToSchema)]
pub(crate) struct DataAgeBody {
    /// True exactly when no stream feeds the definition: it reads only live data.
    live: bool,
    /// The oldest `last_success_at` across the sources whose enabled streams have a mapping that
    /// writes a trait, event or list the definition reads, including through segments it
    /// references. Null when `live`, and null when any feeding source has never succeeded: the
    /// age is then unknown, and `sources` shows which source has no success.
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    as_of: Option<OffsetDateTime>,
    sources: Vec<DataSourceBody>,
}

impl From<DataAge> for DataAgeBody {
    fn from(age: DataAge) -> Self {
        Self {
            live: age.live,
            as_of: age.as_of,
            sources: age
                .sources
                .into_iter()
                .map(|source| DataSourceBody {
                    source_id: source.source_id,
                    name: source.name,
                    last_success_at: source.last_success_at,
                })
                .collect(),
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct PreviewBody {
    /// How many people match.
    count: i64,
    sample: Vec<SampleBody>,
    data_age: DataAgeBody,
    /// The database time the definition was compiled at, which fixes every relative window.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    compiled_at: OffsetDateTime,
}

fn sample_size(requested: Option<u32>) -> Result<u32, ApiError> {
    match requested {
        None => Ok(DEFAULT_SAMPLE),
        Some(size) if (1..=MAX_SAMPLE).contains(&size) => Ok(size),
        Some(_) => Err(ApiError::unprocessable(format!(
            "sample_size: must be from 1 to {MAX_SAMPLE}"
        ))),
    }
}

fn param(param: Param) -> SqlParam {
    match param {
        Param::Text(value) => SqlParam::Text(value),
        Param::TextArray(values) => SqlParam::TextArray(values),
        Param::Float(value) => SqlParam::Float(value),
        Param::FloatArray(values) => SqlParam::FloatArray(values),
        Param::Int(value) => SqlParam::Int(value),
        Param::Timestamp(value) => SqlParam::Timestamp(value),
        Param::Uuid(value) => SqlParam::Uuid(value),
        Param::UuidArray(values) => SqlParam::UuidArray(values),
        Param::Bool(value) => SqlParam::Bool(value),
        Param::Jsonb(value) => SqlParam::Jsonb(value),
    }
}

pub(crate) fn sql_query(query: Query) -> SqlQuery {
    SqlQuery {
        sql: query.sql,
        params: query.params.into_iter().map(param).collect(),
    }
}

/// A read past its time limit is `segment_preview_timeout`, never a 500.
pub(super) fn read_error(error: StoreError) -> ApiError {
    if error.is_timeout() {
        ApiError::segment_preview_timeout()
    } else {
        error.into()
    }
}

/// Previews a definition without saving it: how many people match, a sample of them and the age
/// of the data. A definition that does not validate is 422 `segment_invalid` with its issues. A
/// preview that runs past its time limit is 503 with the code `segment_preview_timeout`.
/// Identifier values in the sample are shown to the owner, workspace admins and `admin` keys and
/// masked for everyone else.
#[utoipa::path(
    post,
    operation_id = "preview_segment",
    path = "/api/v1/workspaces/{ws}/segments/preview",
    tag = "segments",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = PreviewRequest,
    responses(
        (status = 200, description = "The count, a sample and the data age", body = PreviewBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "A definition that does not validate (`code` is `segment_invalid`, with `issues`), or a sample size out of range", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The preview ran past its time limit (`code` is `segment_preview_timeout`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn preview(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<PreviewRequest>,
) -> Result<Json<PreviewBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let size = sample_size(body.sample_size)?;
    let reveal = may_see_values(&state, &principal, workspace.id).await;

    let mut tx =
        zirv_flow_store::begin_segment_snapshot(&state.pool, state.segment_preview_timeout)
            .await
            .map_err(read_error)?;
    let compiled_at = state.clock.now(&mut tx).await.map_err(read_error)?;
    let catalog = catalog_of(
        zirv_flow_store::segment_catalog(&mut tx, workspace.id)
            .await
            .map_err(read_error)?,
    );
    let definition = parse_and_validate(&body.definition, &catalog).map_err(issues_error)?;
    let compiled = compile(&definition, &catalog, &TraitStorage::default(), compiled_at)
        .map_err(issues_error)?;
    let (count, sample) = zirv_flow_store::run_segment_preview(
        &mut tx,
        workspace.id,
        &sql_query(compiled.count_sql()),
        &sql_query(compiled.sample_sql(size)),
    )
    .await
    .map_err(read_error)?;
    let data_age =
        zirv_flow_store::segment_data_age(&mut tx, workspace.id, &compiled.deps.to_value())
            .await
            .map_err(read_error)?;
    drop(tx);

    Ok(Json(PreviewBody {
        count,
        sample: sample
            .into_iter()
            .map(|row| SampleBody {
                id: row.id,
                display: row
                    .display
                    .map(|(kind, value)| DisplayBody::new(kind, value, reveal)),
            })
            .collect(),
        data_age: data_age.into(),
        compiled_at,
    }))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use zirv_flow_store::FeedingSource;

    use super::*;

    #[test]
    fn the_sample_size_defaults_to_20_and_is_held_to_1_through_100() {
        assert_eq!(sample_size(None).unwrap(), 20);
        assert_eq!(sample_size(Some(1)).unwrap(), 1);
        assert_eq!(sample_size(Some(100)).unwrap(), 100);
        for size in [0, 101] {
            let error = sample_size(Some(size)).unwrap_err();
            assert_eq!(error.status(), StatusCode::UNPROCESSABLE_ENTITY);
        }
    }

    #[test]
    fn every_compiler_value_becomes_the_same_store_value() {
        let id = Uuid::nil();
        let query = sql_query(Query {
            sql: "SELECT 1".into(),
            params: vec![
                Param::Text("a".into()),
                Param::TextArray(vec!["b".into()]),
                Param::Float(1.5),
                Param::FloatArray(vec![2.5]),
                Param::Int(3),
                Param::Timestamp(OffsetDateTime::UNIX_EPOCH),
                Param::Uuid(id),
                Param::UuidArray(vec![id]),
                Param::Bool(true),
                Param::Jsonb(serde_json::json!({"k": 1})),
            ],
        });

        assert_eq!(query.sql, "SELECT 1");
        assert_eq!(
            query.params,
            [
                SqlParam::Text("a".into()),
                SqlParam::TextArray(vec!["b".into()]),
                SqlParam::Float(1.5),
                SqlParam::FloatArray(vec![2.5]),
                SqlParam::Int(3),
                SqlParam::Timestamp(OffsetDateTime::UNIX_EPOCH),
                SqlParam::Uuid(id),
                SqlParam::UuidArray(vec![id]),
                SqlParam::Bool(true),
                SqlParam::Jsonb(serde_json::json!({"k": 1})),
            ]
        );
    }

    #[test]
    fn the_data_age_body_carries_the_oldest_time_and_each_source() {
        let source = Uuid::now_v7();
        let body = DataAgeBody::from(DataAge {
            live: false,
            as_of: Some(OffsetDateTime::UNIX_EPOCH),
            sources: vec![FeedingSource {
                source_id: source,
                name: "CRM".into(),
                last_success_at: None,
            }],
        });

        let json = serde_json::to_value(body).unwrap();

        assert_eq!(json["as_of"], "1970-01-01T00:00:00Z");
        assert_eq!(json["sources"][0]["name"], "CRM");
        assert!(json["sources"][0]["last_success_at"].is_null());
        let live = serde_json::to_value(DataAgeBody::from(DataAge {
            live: true,
            as_of: None,
            sources: Vec::new(),
        }))
        .unwrap();
        assert!(live["as_of"].is_null());
        assert_eq!(live["live"], true);
        assert_eq!(json["live"], false);
    }

    #[test]
    fn a_timeout_is_a_503_with_its_own_code() {
        let error = ApiError::segment_preview_timeout();

        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error.code(), Some("segment_preview_timeout"));
    }
}
