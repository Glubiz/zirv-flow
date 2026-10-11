//! The imports of a workspace: the history, one import's state and counters (polled for
//! progress), its mapping, and starting, running and cancelling its passes. Audit entries hold
//! ids and counts only.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use serde::Serialize;
use serde_json::json;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::csv_file::CsvParser;
use zirv_flow_core::mapping::{self, Header, MappingError, TraitState};
use zirv_flow_core::{TraceContext, WorkspaceId};
use zirv_flow_store::{
    DRY_RUN_JOB_KIND, Import, PgPool, RUN_JOB_KIND, StoreError, cancel_import, enqueue_import_job,
    find_import, find_list, list_imports, list_trait_definitions, read_import_chunk,
    set_import_mapping, start_import_dry_run, start_import_run,
};

use super::{CsvImportBody, CsvImportDetailBody, MappingBody, SAMPLE_ROWS, transition_error};
use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, may_see_values, path_id};
use crate::jobs_admin::{encode_cursor, parse_page};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};
use crate::trace::request_trace;

#[derive(Serialize, ToSchema)]
pub(crate) struct CsvImportsBody {
    /// Newest first.
    imports: Vec<CsvImportBody>,
    /// Pass as `before` for the next page; null when there is none.
    next_before: Option<String>,
}

/// The workspace's imports, newest first: the run history.
#[utoipa::path(
    get,
    operation_id = "list_imports",
    path = "/api/v1/workspaces/{ws}/imports",
    tag = "imports",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("limit" = Option<u32>, Query, description = "Imports per page, 1 to 200; default 50"),
        ("before" = Option<String>, Query, description = "The next_before of the previous page")
    ),
    responses(
        (status = 200, description = "A page of imports", body = CsvImportsBody),
        (status = 400, description = "limit is out of range or before is malformed", body = Problem, content_type = "application/problem+json"),
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
) -> Result<Json<CsvImportsBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let (limit, before) = parse_page(&query.unwrap_or_default())?;
    let (imports, more) =
        list_imports(&state.pool, workspace.id, before.map(|(_, id)| id), limit).await?;
    let next_before = match imports.last() {
        Some(last) if more => Some(encode_cursor((last.created_at, last.id))),
        _ => None,
    };
    Ok(Json(CsvImportsBody {
        imports: imports.iter().map(CsvImportBody::from).collect(),
        next_before,
    }))
}

/// Whether the query string asks for the sample rows.
fn wants_samples(query: &str) -> bool {
    url::form_urlencoded::parse(query.as_bytes())
        .any(|(key, value)| key == "samples" && value == "true")
}

/// The first data rows of an import's stored file; none once the file was deleted.
async fn sample_rows(pool: &PgPool, import: &Import) -> Result<Vec<Vec<String>>, StoreError> {
    let mut parser = CsvParser::new(u8::try_from(import.delimiter).unwrap_or(b','));
    let mut records: Vec<Vec<String>> = Vec::new();
    let mut seq = 0;
    while records.len() <= SAMPLE_ROWS {
        let Some(chunk) = read_import_chunk(pool, import.id, seq).await? else {
            break;
        };
        seq += 1;
        if parser
            .push(&chunk, &mut |_, record| records.push(record))
            .is_err()
        {
            return Ok(Vec::new());
        }
    }
    if records.len() <= SAMPLE_ROWS {
        let _ = parser.finish(&mut |_, record| records.push(record));
    }
    // The first record is the header.
    Ok(records.into_iter().skip(1).take(SAMPLE_ROWS).collect())
}

/// One import: its state and counters, which a client polls for progress. With
/// `samples=true` it also shows the first 20 rows of the file, masked for callers who may not
/// see values.
#[utoipa::path(
    get,
    operation_id = "get_import",
    path = "/api/v1/workspaces/{ws}/imports/{id}",
    tag = "imports",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Import id"),
        ("samples" = Option<bool>, Query, description = "Include the first 20 rows of the file")
    ),
    responses(
        (status = 200, description = "The import", body = CsvImportDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or import, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Json<CsvImportDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let import = find_import(&state.pool, workspace.id, path_id(&id)?)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let samples = if wants_samples(&query.unwrap_or_default()) {
        sample_rows(&state.pool, &import).await?
    } else {
        Vec::new()
    };
    let may_see = may_see_values(&state, &principal, workspace.id).await;
    Ok(Json(CsvImportDetailBody::new(&import, samples, may_see)))
}

/// The mapping problem as one line: its code, and the column when it is on one.
fn mapping_problem(error: &MappingError) -> ApiError {
    let detail = match error.column() {
        Some(column) => format!("mapping: {} (column \"{column}\")", error.code()),
        None => format!("mapping: {}", error.code()),
    };
    ApiError::unprocessable(detail)
}

/// Saves the mapping and moves the import to `mapped`. The mapping needs at least one
/// identifier target, columns the file has, and trait keys that exist and are not archived;
/// list targets must exist. Allowed until the first run starts; saving clears the counters and
/// rejects of an earlier dry run.
#[utoipa::path(
    put,
    operation_id = "put_import_mapping",
    path = "/api/v1/workspaces/{ws}/imports/{id}/mapping",
    tag = "imports",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Import id")
    ),
    request_body = MappingBody,
    responses(
        (status = 200, description = "The mapped import", body = CsvImportBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or import, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The import is running or has ended", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The mapping is invalid; the detail names the rule and the column", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn put_mapping(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<MappingBody>,
) -> Result<Json<CsvImportBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    let id: Uuid = path_id(&id)?;
    let import = find_import(&state.pool, workspace.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let mapping = body.to_core()?;
    let header = Header::new(import.header.clone()).map_err(|_| ApiError::internal())?;
    let catalog: std::collections::HashMap<String, TraitState> =
        list_trait_definitions(&state.pool, workspace.id)
            .await?
            .into_iter()
            .map(|definition| {
                let trait_state = if definition.archived {
                    TraitState::Archived
                } else {
                    TraitState::Active(definition.trait_type)
                };
                (definition.key, trait_state)
            })
            .collect();
    mapping::validate(&mapping, &header, &catalog).map_err(|error| mapping_problem(&error))?;
    for rule in &mapping.columns {
        if let mapping::Target::List { list_id } = &rule.target
            && find_list(&state.pool, workspace.id, *list_id)
                .await?
                .is_none()
        {
            return Err(ApiError::unprocessable(format!(
                "mapping: unknown_list (column \"{}\")",
                rule.column
            )));
        }
    }
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let saved = set_import_mapping(&mut tx, workspace.id, id, &mapping)
        .await
        .map_err(transition_error)?;
    zirv_flow_store::commit(tx).await?;
    Ok(Json(CsvImportBody::from(&saved)))
}

/// Enqueues the job of a pass in the transaction that started it. A job for the same import
/// and pass that is still queued, running or dead holds the key: the start is refused and
/// rolled back.
async fn enqueue_pass(
    tx: &mut zirv_flow_store::Tx,
    kind: &'static str,
    workspace: WorkspaceId,
    import: Uuid,
    trace: &TraceContext,
) -> Result<(), ApiError> {
    if enqueue_import_job(&mut **tx, kind, workspace, import, trace).await? {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::CONFLICT,
            "a job for this import is still queued or dead; retry it from the jobs page",
        ))
    }
}

/// Starts the dry run: maps every row of the file and keeps only the rejects (phase
/// `dry_run`) and the counters; no person, list or trait is written. The answer is the import
/// in `dry_running`; poll it until `dry_run_done`. Needs a saved mapping.
#[utoipa::path(
    post,
    operation_id = "dry_run_import",
    path = "/api/v1/workspaces/{ws}/imports/{id}/dry-run",
    tag = "imports",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Import id")
    ),
    responses(
        (status = 202, description = "The dry run was queued", body = CsvImportBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or import, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The import has no mapping, is busy, or has ended", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn dry_run(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<(StatusCode, Json<CsvImportBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    let id: Uuid = path_id(&id)?;
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let import = start_import_dry_run(&mut tx, workspace.id, id)
        .await
        .map_err(transition_error)?;
    enqueue_pass(
        &mut tx,
        DRY_RUN_JOB_KIND,
        workspace.id,
        id,
        &request_trace(),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((StatusCode::ACCEPTED, Json(CsvImportBody::from(&import))))
}

/// Starts the run: applies every row in batches of 500, each batch one transaction that
/// writes the profile records, adds the list members, keeps the rejects and advances the
/// checkpoint. A crash resumes from the checkpoint. The run fails with `reject_threshold` once
/// more than 1,000 rows are done and the rejected share is above the import's threshold; the
/// batches already applied stay applied. Running the same file again with the same source and
/// stream changes nothing. The answer is the import in `running`; poll it. Audited as
/// `import.run_started`.
#[utoipa::path(
    post,
    operation_id = "run_import",
    path = "/api/v1/workspaces/{ws}/imports/{id}/run",
    tag = "imports",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Import id")
    ),
    responses(
        (status = 202, description = "The run was queued", body = CsvImportBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or import, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The import has no mapping, is busy, or has ended", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn run(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<(StatusCode, Json<CsvImportBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    let id: Uuid = path_id(&id)?;
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let import = start_import_run(&mut tx, workspace.id, id)
        .await
        .map_err(transition_error)?;
    enqueue_pass(&mut tx, RUN_JOB_KIND, workspace.id, id, &request_trace()).await?;
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "import.run_started",
        Some(("import", id)),
        json!({ "rows": import.rows_total }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((StatusCode::ACCEPTED, Json(CsvImportBody::from(&import))))
}

/// Cancels an import that has not ended. A running pass stops at its next batch boundary, so
/// the batches already applied stay applied. Audited as `import.cancelled`.
#[utoipa::path(
    post,
    operation_id = "cancel_import",
    path = "/api/v1/workspaces/{ws}/imports/{id}/cancel",
    tag = "imports",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Import id")
    ),
    responses(
        (status = 200, description = "The cancelled import", body = CsvImportBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or import, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The import has ended already", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn cancel(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<CsvImportBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    let id: Uuid = path_id(&id)?;
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let import = cancel_import(&mut tx, workspace.id, id)
        .await
        .map_err(transition_error)?;
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "import.cancelled",
        Some(("import", id)),
        json!({ "rows_done": import.rows_done, "rows": import.rows_total }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(Json(CsvImportBody::from(&import)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_are_asked_for_with_samples_true_only() {
        assert!(wants_samples("samples=true"));
        assert!(wants_samples("x=1&samples=true"));
        assert!(!wants_samples(""));
        assert!(!wants_samples("samples=false"));
        assert!(!wants_samples("samples=1"));
    }

    #[test]
    fn a_mapping_problem_names_the_rule_and_the_column_and_nothing_else() {
        let on_column = mapping_problem(&MappingError::UnknownTrait("Plan".into()));
        let general = mapping_problem(&MappingError::NoIdentifier);

        assert_eq!(on_column.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            on_column.detail(),
            "mapping: unknown_trait (column \"Plan\")"
        );
        assert_eq!(general.detail(), "mapping: no_identifier");
    }
}
