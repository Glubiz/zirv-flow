//! The rows an import refused: row numbers and codes as JSON, and as a CSV that also holds
//! the rows' own cells for the owner and admins. The cells are personal data, so that file is
//! the one place they come back, and every download is audited.

use std::io;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, RawQuery, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use futures_util::stream;
use serde::Serialize;
use serde_json::json;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::csv_file::write_field;
use zirv_flow_store::{ImportPhase, ImportReject, PgPool, find_import, list_import_rejects};

use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, may_see_values, path_id};
use crate::problem::{ApiError, Problem};

const DEFAULT_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 1000;
/// Rejects per page of the CSV download.
const DOWNLOAD_PAGE: u32 = 1000;

#[derive(Serialize, ToSchema)]
pub(crate) struct ImportRejectErrorBody {
    /// The column the problem is on; empty when none.
    column: String,
    /// `missing_identifier`, `invalid_identifier`, `wrong_type`, `bad_timestamp`,
    /// `unknown_column`, `invalid_key` or `apply_failed`.
    reason: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ImportRejectBody {
    /// The row as a spreadsheet numbers it: the header is row 1.
    row_number: i64,
    /// `dry_run` or `run`.
    phase: String,
    errors: Vec<ImportRejectErrorBody>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ImportRejectsBody {
    /// In row order.
    rejects: Vec<ImportRejectBody>,
    /// Pass as `after` for the next page; null when there is none.
    next_after: Option<i64>,
}

impl From<&ImportReject> for ImportRejectBody {
    fn from(reject: &ImportReject) -> Self {
        Self {
            row_number: reject.row_number,
            phase: reject.phase.clone(),
            errors: reject
                .errors
                .iter()
                .map(|error| ImportRejectErrorBody {
                    column: error.column.clone(),
                    reason: error.reason.clone(),
                })
                .collect(),
        }
    }
}

struct Page {
    phase: Option<ImportPhase>,
    limit: u32,
    after: Option<i64>,
}

fn parse_phase(value: &str) -> Result<ImportPhase, ApiError> {
    ImportPhase::parse(value).ok_or_else(|| ApiError::bad_request("phase must be dry_run or run"))
}

fn parse_page(query: &str) -> Result<Page, ApiError> {
    let mut page = Page {
        phase: None,
        limit: DEFAULT_LIMIT,
        after: None,
    };
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "phase" => page.phase = Some(parse_phase(&value)?),
            "limit" => {
                page.limit = value
                    .parse::<u32>()
                    .ok()
                    .filter(|limit| (1..=MAX_LIMIT).contains(limit))
                    .ok_or_else(|| {
                        ApiError::bad_request(format!("limit must be from 1 to {MAX_LIMIT}"))
                    })?;
            }
            "after" => {
                page.after = Some(
                    value
                        .parse::<i64>()
                        .ok()
                        .filter(|row| *row >= 0)
                        .ok_or_else(|| ApiError::bad_request("after must be a row number"))?,
                );
            }
            _ => {}
        }
    }
    Ok(page)
}

/// The rows an import refused, in row order, with the codes of what was wrong: never a cell.
/// `phase` narrows to the dry run's or the run's rejects. Rejects are kept 30 days.
#[utoipa::path(
    get,
    operation_id = "list_import_rejects",
    path = "/api/v1/workspaces/{ws}/imports/{id}/rejects",
    tag = "imports",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Import id"),
        ("phase" = Option<String>, Query, description = "dry_run or run"),
        ("limit" = Option<u32>, Query, description = "Rejects per page, 1 to 1000; default 100"),
        ("after" = Option<i64>, Query, description = "The next_after of the previous page")
    ),
    responses(
        (status = 200, description = "A page of rejects", body = ImportRejectsBody),
        (status = 400, description = "A malformed phase, limit or after", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or import, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn list(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Json<ImportRejectsBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let id: Uuid = path_id(&id)?;
    let page = parse_page(&query.unwrap_or_default())?;
    find_import(&state.pool, workspace.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let (rejects, more) = list_import_rejects(
        &state.pool,
        workspace.id,
        id,
        page.phase,
        page.after,
        page.limit,
        false,
    )
    .await?;
    let next_after = match rejects.last() {
        Some(last) if more => Some(last.row_number),
        _ => None,
    };
    Ok(Json(ImportRejectsBody {
        rejects: rejects.iter().map(ImportRejectBody::from).collect(),
        next_after,
    }))
}

/// A cell of the download. A cell that a spreadsheet would read as a formula gets a leading
/// quote, so opening the file never runs one.
fn download_cell(out: &mut String, cell: &str) {
    if cell.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        write_field(out, &format!("'{cell}"), ',');
    } else {
        write_field(out, cell, ',');
    }
}

/// The header line of the download; with cells, the file's own column names follow.
fn download_header(file_header: &[String], with_cells: bool) -> String {
    let mut out = String::from("row_number,column,reason");
    if with_cells {
        for name in file_header {
            out.push(',');
            write_field(&mut out, name, ',');
        }
    }
    out.push('\n');
    out
}

/// The lines of `rejects`: one per error, `row_number,column,reason`, and for the first error
/// of a row its cells after them.
fn download_lines(rejects: &[ImportReject], with_cells: bool) -> String {
    let mut out = String::new();
    for reject in rejects {
        for (position, error) in reject.errors.iter().enumerate() {
            out.push_str(&reject.row_number.to_string());
            out.push(',');
            write_field(&mut out, &error.column, ',');
            out.push(',');
            write_field(&mut out, &error.reason, ',');
            if with_cells && position == 0 {
                for cell in reject.raw.iter().flatten() {
                    out.push(',');
                    download_cell(&mut out, cell);
                }
            }
            out.push('\n');
        }
    }
    out
}

enum Download {
    Header,
    Page(Option<i64>),
    Done,
}

/// The rejects as a CSV, `row_number,column,reason` with one line per problem, streamed page
/// by page. For the owner, workspace admins and `admin` keys the rows' own cells follow, under
/// the file's column names; cells that start like a spreadsheet formula get a leading quote.
/// Everyone else with read access gets the codes alone. Audited as `import.rejects_downloaded`.
#[utoipa::path(
    get,
    operation_id = "download_import_rejects",
    path = "/api/v1/workspaces/{ws}/imports/{id}/rejects.csv",
    tag = "imports",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Import id"),
        ("phase" = Option<String>, Query, description = "dry_run or run")
    ),
    responses(
        (status = 200, description = "The rejects as CSV", body = String, content_type = "text/csv"),
        (status = 400, description = "A malformed phase", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or import, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn download(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let id: Uuid = path_id(&id)?;
    let phase = parse_page(&query.unwrap_or_default())?.phase;
    let import = find_import(&state.pool, workspace.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let with_cells = may_see_values(&state, &principal, workspace.id).await;
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "import.rejects_downloaded",
        Some(("import", id)),
        json!({
            "rows_rejected": import.rows_rejected,
            "phase": phase.map(ImportPhase::as_str),
            "with_cells": with_cells,
        }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;

    let (pool, workspace_id, file_header) =
        (state.pool.clone(), workspace.id, import.header.clone());
    let body = stream::unfold(Download::Header, move |step| {
        let (pool, file_header) = (pool.clone(), file_header.clone());
        async move {
            match step {
                Download::Header => Some((
                    Ok(Bytes::from(download_header(&file_header, with_cells))),
                    Download::Page(None),
                )),
                Download::Page(after) => {
                    match next_page(&pool, workspace_id, id, phase, after, with_cells).await {
                        Ok((bytes, next)) => Some((Ok(bytes), next)),
                        Err(error) => {
                            tracing::error!(%error, "the rejects download failed");
                            Some((Err(io::Error::other("the download failed")), Download::Done))
                        }
                    }
                }
                Download::Done => None,
            }
        }
    });
    let mut response = Response::new(Body::from_stream(body));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        "text/csv; charset=utf-8".parse().unwrap(),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        "attachment; filename=\"rejects.csv\"".parse().unwrap(),
    );
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    Ok(response)
}

async fn next_page(
    pool: &PgPool,
    workspace: WorkspaceId,
    import: Uuid,
    phase: Option<ImportPhase>,
    after: Option<i64>,
    with_cells: bool,
) -> Result<(Bytes, Download), zirv_flow_store::StoreError> {
    let (page, more) = list_import_rejects(
        pool,
        workspace,
        import,
        phase,
        after,
        DOWNLOAD_PAGE,
        with_cells,
    )
    .await?;
    let next = match page.last() {
        Some(last) if more => Download::Page(Some(last.row_number)),
        _ => Download::Done,
    };
    Ok((Bytes::from(download_lines(&page, with_cells)), next))
}

#[cfg(test)]
mod tests {
    use zirv_flow_store::StoredReason;

    use super::*;

    fn reject(row: i64, errors: &[(&str, &str)], raw: Option<&[&str]>) -> ImportReject {
        ImportReject {
            row_number: row,
            phase: "run".into(),
            errors: errors
                .iter()
                .map(|(column, reason)| StoredReason {
                    column: (*column).into(),
                    reason: (*reason).into(),
                })
                .collect(),
            raw: raw.map(|cells| cells.iter().map(|c| (*c).to_owned()).collect()),
        }
    }

    #[test]
    fn the_page_is_read_from_the_query_string() {
        let page = parse_page("phase=dry_run&limit=5&after=12").unwrap();

        assert_eq!(
            (page.phase, page.limit, page.after),
            (Some(ImportPhase::DryRun), 5, Some(12))
        );
        let defaults = parse_page("").unwrap();
        assert_eq!(
            (defaults.phase, defaults.limit, defaults.after),
            (None, 100, None)
        );
    }

    #[test]
    fn a_bad_page_is_a_400() {
        for bad in [
            "phase=never",
            "limit=0",
            "limit=1001",
            "limit=x",
            "after=-1",
            "after=x",
        ] {
            assert_eq!(
                parse_page(bad).err().unwrap().status(),
                StatusCode::BAD_REQUEST,
                "{bad}"
            );
        }
    }

    #[test]
    fn the_download_has_one_line_per_error_and_the_cells_only_on_the_first() {
        let rejects = [
            reject(
                7,
                &[("Email", "invalid_identifier"), ("Age", "wrong_type")],
                Some(&["a@b", "old"]),
            ),
            reject(9, &[("", "apply_failed")], Some(&["x, y", "1"])),
        ];

        assert_eq!(
            download_lines(&rejects, true),
            "7,Email,invalid_identifier,a@b,old\n\
             7,Age,wrong_type\n\
             9,,apply_failed,\"x, y\",1\n"
        );
        assert_eq!(
            download_lines(&rejects, false),
            "7,Email,invalid_identifier\n7,Age,wrong_type\n9,,apply_failed\n"
        );
    }

    #[test]
    fn the_cells_never_reach_a_download_that_did_not_ask_for_them() {
        let rejects = [reject(7, &[("Email", "invalid_identifier")], None)];

        assert!(!download_lines(&rejects, true).contains(",a@b"));
        assert_eq!(
            download_lines(&rejects, true),
            "7,Email,invalid_identifier\n"
        );
    }

    #[test]
    fn a_cell_that_looks_like_a_formula_is_quoted_with_a_leading_apostrophe() {
        let rejects = [reject(
            2,
            &[("A", "wrong_type")],
            Some(&["=HYPERLINK(\"x\")", "+1", "-1", "@a", "plain"]),
        )];

        assert_eq!(
            download_lines(&rejects, true),
            "2,A,wrong_type,\"'=HYPERLINK(\"\"x\"\")\",'+1,'-1,'@a,plain\n"
        );
    }

    #[test]
    fn the_download_header_names_the_files_columns_for_those_who_get_cells() {
        let file = vec!["Email".to_owned(), "Full, name".to_owned()];

        assert_eq!(
            download_header(&file, true),
            "row_number,column,reason,Email,\"Full, name\"\n"
        );
        assert_eq!(download_header(&file, false), "row_number,column,reason\n");
    }

    #[test]
    fn a_reject_is_shown_with_codes_only() {
        let shown = serde_json::to_value(ImportRejectBody::from(&reject(
            3,
            &[("Email", "invalid_identifier")],
            Some(&["secret"]),
        )))
        .unwrap();

        assert_eq!(
            shown,
            json!({"row_number": 3, "phase": "run",
                   "errors": [{"column": "Email", "reason": "invalid_identifier"}]})
        );
    }
}
