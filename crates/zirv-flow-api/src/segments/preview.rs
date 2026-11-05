//! Segment preview (architecture 8.3 and 7.5): the count and a sample of the people an unsaved
//! definition matches, and how old the data behind them is. It runs on the primary, in one
//! read-only snapshot under a fixed 20 second statement timeout.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use utoipa::ToSchema;
use utoipa::openapi::{Ref, RefOr, Schema};
use uuid::Uuid;
use zirv_flow_segments::{Param, PreviewQuery, Query, compile, parse_and_validate};
use zirv_flow_store::{DataAge, PreviewSql, SqlParam, SqlQuery, StoreError};

use super::{catalog_of, issues_error};
use crate::AppState;
use crate::auth::{Need, Principal, authorize, may_view_pii, path_id};
use crate::json::ApiJson;
use crate::people::search::DisplayBody;
use crate::problem::{ApiError, Problem};

const DEFAULT_SAMPLE: u32 = 20;
const MAX_SAMPLE: u32 = 100;

fn definition_schema() -> RefOr<Schema> {
    RefOr::Ref(Ref::from_schema_name(super::schema::DEFINITION))
}

#[derive(Deserialize, ToSchema)]
#[schema(as = SegmentPreviewRequest)]
pub(crate) struct PreviewRequest {
    /// The definition to preview, saved or not.
    #[schema(schema_with = definition_schema)]
    definition: Value,
    /// People to sample, 1 to 100; default 20. The sample is the first people by id.
    sample_size: Option<u32>,
}

#[derive(Serialize, ToSchema)]
#[schema(as = SegmentSampleBody)]
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
#[schema(as = SegmentPreviewBody)]
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

/// The store's form of the statements a preview runs.
fn preview_sql(query: PreviewQuery) -> PreviewSql {
    match query {
        PreviewQuery::Grouped(query) => PreviewSql::Grouped(sql_query(query)),
        PreviewQuery::Ungrouped { count, sample } => PreviewSql::Ungrouped {
            count: sql_query(count),
            sample: sql_query(sample),
        },
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
    let reveal = may_view_pii(&state, &principal, workspace.id).await;

    let mut tx =
        zirv_flow_store::begin_segment_snapshot(&state.pool, state.segment_preview_timeout)
            .await
            .map_err(read_error)?;
    let compiled_at = state.clock.now(&mut tx).await.map_err(read_error)?;
    let (catalog, storage) = catalog_of(
        zirv_flow_store::segment_catalog(&mut tx, workspace.id)
            .await
            .map_err(read_error)?,
    );
    let definition = parse_and_validate(&body.definition, &catalog).map_err(issues_error)?;
    let compiled = compile(&definition, &catalog, &storage, compiled_at).map_err(issues_error)?;
    let (count, sample) = zirv_flow_store::run_segment_preview(
        &mut tx,
        workspace.id,
        &preview_sql(compiled.preview_sql(size)),
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
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::http::StatusCode;
    use hashbrown::HashMap;
    use serde_json::json;
    use sqlx::PgPool;
    use sqlx::postgres::{PgPoolOptions, PgSslMode};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use zirv_flow_core::people::TraitType;
    use zirv_flow_core::{Slug, WorkspaceId, WorkspaceName};
    use zirv_flow_segments::{Catalog, CompiledSegment, Definition, TraitStorage};
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

    fn catalog() -> Catalog {
        let mut catalog = Catalog::default();
        catalog.traits.insert("plan".to_owned(), TraitType::String);
        for name in ["order", "page_view"] {
            catalog.events.insert(name.to_owned(), HashMap::new());
        }
        catalog
    }

    fn compiled(definition: Value) -> CompiledSegment {
        let mut definition = definition;
        definition["v"] = json!(1);
        let definition = Definition::from_value(&definition).unwrap();
        compile(
            &definition,
            &catalog(),
            &TraitStorage::default(),
            OffsetDateTime::now_utc(),
        )
        .unwrap()
    }

    fn orders_at_least(count: i64) -> Value {
        json!({"event": "order", "window": {"last": "P90D"},
               "agg": {"fn": "count", "op": "gte", "value": count}})
    }

    fn pro() -> Value {
        json!({"trait": "plan", "op": "eq", "value": "pro"})
    }

    /// Migrates, then seeds twelve people, numbered 1 to 12: the first six on `pro`. Person `n` has `n % 3` orders in
    /// the last 90 days, three older ones and two page views. A person of another workspace has
    /// orders too.
    async fn seed(pool: &PgPool) -> WorkspaceId {
        zirv_flow_store::migrate(pool).await.unwrap();
        let mut ids = Vec::new();
        for slug in ["one", "two"] {
            let name = WorkspaceName::parse(slug).unwrap();
            let slug = Slug::parse(slug).unwrap();
            ids.push(
                zirv_flow_store::create_workspace(pool, &name, &slug)
                    .await
                    .unwrap()
                    .id,
            );
        }
        let (ws, other) = (ids[0], ids[1]);
        sqlx::query(
            "INSERT INTO people (id, workspace_id, traits) \
             SELECT md5('one/' || n)::uuid, $1, \
                    jsonb_build_object('plan', CASE WHEN n <= 6 THEN 'pro' ELSE 'free' END) \
             FROM generate_series(1, 12) n \
             UNION ALL SELECT md5('two/1')::uuid, $2, '{\"plan\": \"pro\"}'",
        )
        .bind(ws.as_uuid())
        .bind(other.as_uuid())
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO events (id, workspace_id, person_id, name, occurred_at, received_at, \
                 source, trace_id, validation_state) \
             SELECT gen_random_uuid(), p.workspace_id, p.id, e.name, e.at, now(), 'web', \
                    gen_random_uuid(), 'valid' \
             FROM (SELECT n, md5('one/' || n)::uuid AS id, $1::uuid AS workspace_id \
                   FROM generate_series(1, 12) n \
                   UNION ALL SELECT 5, md5('two/1')::uuid, $2::uuid) p \
             CROSS JOIN LATERAL ( \
                 SELECT 'order', now() - k * interval '1 day' FROM generate_series(1, p.n % 3) k \
                 UNION ALL SELECT 'order', now() - (200 + k) * interval '1 day' \
                     FROM generate_series(1, 3) k \
                 UNION ALL SELECT 'page_view', now() - k * interval '1 hour' \
                     FROM generate_series(1, 2) k) e(name, at)",
        )
        .bind(ws.as_uuid())
        .bind(other.as_uuid())
        .execute(pool)
        .await
        .unwrap();
        ws
    }

    /// The ids of the people numbered `numbers`, in id order.
    async fn people(pool: &PgPool, numbers: &[i32]) -> Vec<Uuid> {
        sqlx::query_scalar("SELECT md5('one/' || n)::uuid FROM unnest($1::int4[]) n ORDER BY 1")
            .bind(numbers)
            .fetch_all(pool)
            .await
            .unwrap()
    }

    async fn run(
        pool: &PgPool,
        ws: WorkspaceId,
        preview: &PreviewSql,
    ) -> (i64, Vec<zirv_flow_store::SampleRow>) {
        let mut tx = zirv_flow_store::begin_segment_snapshot(pool, Duration::from_secs(10))
            .await
            .unwrap();
        zirv_flow_store::run_segment_preview(&mut tx, ws, preview)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_preview_counts_and_samples_as_a_count_and_a_sample_statement_do(pool: PgPool) {
        let ws = seed(&pool).await;
        let negated = json!({"not": orders_at_least(2)});
        let both = json!({"all": [pro(), orders_at_least(2)]});
        // A sample smaller than the members, one smaller than its limit, an empty segment, a
        // negated aggregate (a joined set), a trait and an aggregate, and a trait alone.
        let cases: [(Value, u32, &[i32]); 5] = [
            (orders_at_least(2), 3, &[2, 5, 8, 11]),
            (both, 20, &[2, 5]),
            (orders_at_least(100), 20, &[]),
            (negated, 20, &[1, 3, 4, 6, 7, 9, 10, 12]),
            (pro(), 20, &[1, 2, 3, 4, 5, 6]),
        ];

        for (definition, size, members) in cases {
            let mut sample = people(&pool, members).await;
            sample.truncate(usize::try_from(size).unwrap());
            let compiled = compiled(definition.clone());
            let previewed = run(&pool, ws, &preview_sql(compiled.preview_sql(size))).await;
            let reference = PreviewSql::Ungrouped {
                count: sql_query(compiled.count_sql()),
                sample: sql_query(compiled.sample_sql(size)),
            };

            assert_eq!(previewed, run(&pool, ws, &reference).await, "{definition}");
            assert_eq!(
                previewed.0,
                i64::try_from(members.len()).unwrap(),
                "{definition}"
            );
            let ids: Vec<Uuid> = previewed.1.iter().map(|row| row.id).collect();
            assert_eq!(ids, sample, "{definition}");
        }
    }

    /// The SQL of every statement sent through the returned pool, read off the wire: a TCP relay
    /// to the test server keeps the text of each Parse and simple Query message.
    async fn relay(pool: &PgPool) -> (PgPool, Arc<Mutex<Vec<String>>>) {
        let target = pool.connect_options();
        let upstream = format!("{}:{}", target.get_host(), target.get_port());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&sent);
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let server = TcpStream::connect(&upstream).await.unwrap();
                tokio::spawn(forward(client, server, Arc::clone(&log)));
            }
        });
        let options = (*target)
            .clone()
            .host("127.0.0.1")
            .port(port)
            .ssl_mode(PgSslMode::Disable);
        let relayed = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        (relayed, sent)
    }

    async fn forward(client: TcpStream, server: TcpStream, log: Arc<Mutex<Vec<String>>>) {
        let (mut from_client, mut to_client) = client.into_split();
        let (mut from_server, mut to_server) = server.into_split();
        tokio::spawn(async move { tokio::io::copy(&mut from_server, &mut to_client).await });
        let length = |bytes: [u8; 4]| usize::try_from(u32::from_be_bytes(bytes)).unwrap() - 4;
        // The startup message is a length and a body, without a type byte.
        let mut head = [0; 4];
        if from_client.read_exact(&mut head).await.is_err() {
            return;
        }
        let mut body = vec![0; length(head)];
        from_client.read_exact(&mut body).await.unwrap();
        to_server.write_all(&head).await.unwrap();
        to_server.write_all(&body).await.unwrap();
        let mut head = [0; 5];
        while from_client.read_exact(&mut head).await.is_ok() {
            let mut body = vec![0; length([head[1], head[2], head[3], head[4]])];
            from_client.read_exact(&mut body).await.unwrap();
            // A Parse is a statement name and then its text; a simple Query is its text.
            let mut parts = body.split(|byte| *byte == 0);
            let text = match head[0] {
                b'P' => parts.nth(1),
                b'Q' => parts.next(),
                _ => None,
            };
            if let Some(text) = text {
                log.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(text).into_owned());
            }
            to_server.write_all(&head).await.unwrap();
            to_server.write_all(&body).await.unwrap();
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_aggregate_preview_reads_people_in_one_statement_and_a_trait_preview_in_two(
        pool: PgPool,
    ) {
        let ws = seed(&pool).await;
        let (relayed, sent) = relay(&pool).await;
        let mut statements = Vec::new();

        for definition in [orders_at_least(2), pro()] {
            let preview = preview_sql(compiled(definition).preview_sql(20));
            let mut tx = zirv_flow_store::begin_segment_snapshot(&relayed, Duration::from_secs(10))
                .await
                .unwrap();
            sent.lock().unwrap().clear();
            zirv_flow_store::run_segment_preview(&mut tx, ws, &preview)
                .await
                .unwrap();
            let log = sent.lock().unwrap();
            statements.push(
                log.iter()
                    .filter(|sql| sql.contains("FROM people p"))
                    .count(),
            );
        }

        assert_eq!(statements, [1, 2]);
    }
}
