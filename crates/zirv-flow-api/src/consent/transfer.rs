//! Consent import and export as CSV. A row is one state of one person: the person id, the
//! channel, the topic key (empty for the channel as a whole), the status, and then one
//! `dimension:<key>` column per dimension the file scopes by, holding the value key (empty
//! for all values). A file with only the first four columns, the shape that predates
//! dimensions, still imports and means every row is unscoped.

use std::io;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use futures_util::stream;
use hashbrown::HashMap;
use serde::Serialize;
use serde_json::{Value, json};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::consent::{
    ChangeSource, Channel, ConsentStatus, Dimensions, validate_dimension_key,
};
use zirv_flow_store::{
    ConsentChange, ConsentExportCursor, ConsentExportRow, ConsentOutcome, PgPool,
};

use super::person::check_dimensions;
use super::suppressions::{EXPORT_PAGE, MAX_IMPORT_BYTES};
use super::{bool_param, csv, invalid, origin};
use crate::AppState;
use crate::admin::{self, AdminError, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::problem::{ApiError, Problem};

/// The most data rows an import takes: each row is a full consent write.
const MAX_IMPORT_ROWS: usize = 10_000;
/// The columns every file starts with.
const BASE_COLUMNS: [&str; 4] = ["person_id", "channel", "topic", "status"];
const DIMENSION_PREFIX: &str = "dimension:";

#[derive(Serialize, ToSchema)]
pub(crate) struct ConsentImportRowBody {
    /// The line of the file, the header being line 1.
    row: usize,
    /// `malformed_row`, `invalid_person_id`, `invalid_channel`, `invalid_status`,
    /// `invalid_dimension`, `unknown_topic`, `unknown_dimension`, `unknown_dimension_value`
    /// or `no_such_person`. Never the value.
    reason: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ConsentImportBody {
    dry_run: bool,
    /// Rows that changed a state (or, on a dry run, would).
    changed: usize,
    /// Rows that repeated the status already held.
    unchanged: usize,
    invalid: Vec<ConsentImportRowBody>,
}

/// One data row, read but not yet checked against the workspace.
#[derive(Debug, Eq, PartialEq)]
struct Row {
    person: Uuid,
    channel: Channel,
    topic: Option<String>,
    status: ConsentStatus,
    dimensions: Dimensions,
}

/// The dimension keys named by the header, in column order after the four base columns.
fn parse_header(fields: &[String]) -> Result<Vec<String>, AdminError> {
    let names: Vec<String> = fields
        .iter()
        .map(|name| name.trim().to_lowercase())
        .collect();
    if names.len() < BASE_COLUMNS.len() || names[..BASE_COLUMNS.len()] != BASE_COLUMNS {
        return Err(invalid(
            "header",
            "must start with person_id,channel,topic,status",
        ));
    }
    let mut keys: Vec<String> = Vec::new();
    for name in &names[BASE_COLUMNS.len()..] {
        let key = name
            .strip_prefix(DIMENSION_PREFIX)
            .filter(|key| validate_dimension_key(key).is_ok())
            .ok_or_else(|| {
                invalid(
                    "header",
                    "a column after status must be dimension:<dimension key>",
                )
            })?;
        if keys.iter().any(|seen| seen == key) {
            return Err(invalid("header", "a dimension column appears twice"));
        }
        keys.push(key.to_owned());
    }
    Ok(keys)
}

/// Reads one data row. The error is the code reported for the row.
fn parse_row(fields: &[String], dimension_keys: &[String]) -> Result<Row, &'static str> {
    if fields.len() < BASE_COLUMNS.len() || fields.len() > BASE_COLUMNS.len() + dimension_keys.len()
    {
        return Err("malformed_row");
    }
    let person = fields[0].trim().parse().map_err(|_| "invalid_person_id")?;
    let channel = fields[1].trim().parse().map_err(|_| "invalid_channel")?;
    let status = match fields[3].trim().parse() {
        Ok(ConsentStatus::Unknown) | Err(_) => return Err("invalid_status"),
        Ok(status) => status,
    };
    let topic = Some(fields[2].trim().to_owned()).filter(|topic| !topic.is_empty());
    let mut requested = std::collections::BTreeMap::new();
    for (key, cell) in dimension_keys.iter().zip(&fields[BASE_COLUMNS.len()..]) {
        let value = cell.trim();
        if !value.is_empty() {
            requested.insert(key.clone(), value.to_owned());
        }
    }
    let dimensions = check_dimensions(&requested).map_err(|_| "invalid_dimension")?;
    Ok(Row {
        person,
        channel,
        topic,
        status,
        dimensions,
    })
}

/// Imports a CSV of consent states, one write per row, in one transaction. On a dry run
/// nothing is kept. Bad rows are reported by line number and code and do not stop the rest.
/// A real import is audited as `consent.imported` with counts only. Each written change is
/// recorded in the person history with the source `import`.
pub(crate) async fn import(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    text: &str,
    dry_run: bool,
    actor: Option<Uuid>,
) -> Result<ConsentImportBody, AdminError> {
    let mut records = csv::parse(text).into_iter();
    let header = records
        .next()
        .map(|record| record.fields)
        .unwrap_or_default();
    let dimension_keys = parse_header(&header)?;
    let records: Vec<csv::Record> = records.collect();
    if records.len() > MAX_IMPORT_ROWS {
        return Err(invalid("rows", "must be at most 10000 rows"));
    }

    let topics: HashMap<String, Uuid> = zirv_flow_store::list_topics(pool, workspace, true)
        .await?
        .into_iter()
        .map(|topic| (topic.key, topic.id))
        .collect();
    let trace = crate::trace::request_trace();
    let mut tx = zirv_flow_store::begin(pool).await?;
    let mut body = ConsentImportBody {
        dry_run,
        changed: 0,
        unchanged: 0,
        invalid: Vec::new(),
    };
    for csv::Record { line, fields } in records {
        let reject = |body: &mut ConsentImportBody, reason: &str| {
            body.invalid.push(ConsentImportRowBody {
                row: line,
                reason: reason.to_owned(),
            });
        };
        let row = match parse_row(&fields, &dimension_keys) {
            Ok(row) => row,
            Err(code) => {
                reject(&mut body, code);
                continue;
            }
        };
        let topic = match &row.topic {
            Some(key) => match topics.get(key) {
                Some(id) => Some(*id),
                None => {
                    reject(&mut body, "unknown_topic");
                    continue;
                }
            },
            None => None,
        };
        let outcome = zirv_flow_store::set_consent(
            &mut tx,
            workspace,
            &ConsentChange {
                person: row.person,
                channel: row.channel,
                topic,
                dimensions: row.dimensions,
                status: row.status,
                source: ChangeSource::Import,
                evidence: json!({}),
                occurred_at: OffsetDateTime::now_utc(),
                actor,
            },
            &trace,
        )
        .await?;
        match outcome {
            ConsentOutcome::Changed { .. } => body.changed += 1,
            ConsentOutcome::Unchanged => body.unchanged += 1,
            ConsentOutcome::NoSuchPerson => reject(&mut body, "no_such_person"),
            ConsentOutcome::NoSuchTopic => reject(&mut body, "unknown_topic"),
            ConsentOutcome::NoSuchDimension => reject(&mut body, "unknown_dimension"),
        }
    }
    if !dry_run {
        admin::audit(
            &mut tx,
            context,
            Some(workspace),
            "consent.imported",
            None,
            json!({
                "changed": body.changed,
                "unchanged": body.unchanged,
                "invalid": body.invalid.len(),
            }),
        )
        .await?;
        zirv_flow_store::commit(tx).await?;
    }
    Ok(body)
}

/// The export header for the dimension columns `keys`.
fn export_header(keys: &[String]) -> String {
    let mut columns: Vec<String> = BASE_COLUMNS.iter().map(|name| (*name).to_owned()).collect();
    columns.extend(keys.iter().map(|key| format!("{DIMENSION_PREFIX}{key}")));
    let mut line = columns.join(",");
    line.push('\n');
    line
}

/// The export line of one state, with a cell per dimension column.
fn export_line(row: &ConsentExportRow, keys: &[String]) -> String {
    let stored = row.dimensions.as_object();
    let mut cells = vec![
        row.person_id.to_string(),
        row.channel.clone(),
        csv::field(row.topic_key.as_deref().unwrap_or_default()),
        row.status.clone(),
    ];
    for key in keys {
        let value = stored
            .and_then(|map| map.get(key))
            .and_then(Value::as_str)
            .unwrap_or_default();
        cells.push(csv::field(value));
    }
    let mut line = cells.join(",");
    line.push('\n');
    line
}

enum Export {
    Header,
    Page(Option<ConsentExportCursor>),
    Done,
}

/// Imports consent states from a CSV body (`text/csv`, at most 10 MiB and 10,000 rows) with
/// the header `person_id,channel,topic,status`, optionally followed by `dimension:<key>`
/// columns. A row without dimension values is an unscoped state. With `dry_run=true` nothing
/// is written and the answer says what a commit would do. Invalid rows are reported by line
/// number and code, never by value. Audited as `consent.imported` with counts only.
#[utoipa::path(
    post,
    operation_id = "import_consent",
    path = "/api/v1/workspaces/{ws}/consent/import",
    tag = "consent",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("dry_run" = Option<bool>, Query, description = "Only report what an import would do; default false")
    ),
    request_body(content = String, content_type = "text/csv", description = "person_id,channel,topic,status[,dimension:<key>...] rows"),
    responses(
        (status = 200, description = "What was, or on a dry run would be, imported", body = ConsentImportBody),
        (status = 400, description = "dry_run is not true or false, or the body is not UTF-8", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "The body is over 10 MiB", body = Problem, content_type = "application/problem+json"),
        (status = 415, description = "The content type is not text/csv", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "A wrong header, or more than 10,000 rows", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn import_handler(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<ConsentImportBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let dry_run = bool_param(query.as_deref().unwrap_or_default(), "dry_run")?;
    let is_csv = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim().to_ascii_lowercase().starts_with("text/csv"));
    if !is_csv {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "expected a CSV body (content-type: text/csv)",
        ));
    }
    let bytes = axum::body::to_bytes(body, MAX_IMPORT_BYTES)
        .await
        .map_err(|_| ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "the CSV is over 10 MiB"))?;
    let text =
        std::str::from_utf8(&bytes).map_err(|_| ApiError::bad_request("the CSV must be UTF-8"))?;
    let (_, actor) = origin(&principal);
    let result = import(
        &state.pool,
        &principal.context(),
        workspace.id,
        text,
        dry_run,
        actor,
    )
    .await?;
    Ok(Json(result))
}

/// Every consent state as CSV, `person_id,channel,topic,status` and one `dimension:<key>`
/// column per declared dimension, streamed page by page. The file imports back as it is.
#[utoipa::path(
    get,
    operation_id = "export_consent",
    path = "/api/v1/workspaces/{ws}/consent/export",
    tag = "consent",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The consent states as CSV, ordered by person", body = String, content_type = "text/csv"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or a key without the admin scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn export(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Response, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let (pool, workspace) = (state.pool.clone(), workspace.id);
    let keys: Vec<String> = {
        let mut connection = zirv_flow_store::begin(&pool).await?;
        zirv_flow_store::list_dimensions(&mut connection, workspace, true)
            .await?
            .into_iter()
            .filter(|dimension| !dimension.builtin)
            .map(|dimension| dimension.key)
            .collect()
    };
    let body = stream::unfold(Export::Header, move |phase| {
        let (pool, keys) = (pool.clone(), keys.clone());
        async move {
            match phase {
                Export::Header => Some((Ok(Bytes::from(export_header(&keys))), Export::Page(None))),
                Export::Page(cursor) => {
                    let page = zirv_flow_store::consent_export_page(
                        &pool,
                        workspace,
                        cursor.as_ref(),
                        EXPORT_PAGE,
                    )
                    .await;
                    match page {
                        Ok(page) => {
                            let next = match page.last() {
                                Some(last) if page.len() == EXPORT_PAGE as usize => {
                                    Export::Page(Some(last.cursor()))
                                }
                                _ => Export::Done,
                            };
                            let text: String =
                                page.iter().map(|row| export_line(row, &keys)).collect();
                            Some((Ok(Bytes::from(text)), next))
                        }
                        Err(error) => {
                            tracing::error!(%error, "the consent export failed");
                            Some((Err(io::Error::other("the export failed")), Export::Done))
                        }
                    }
                }
                Export::Done => None,
            }
        }
    });
    let mut response = Response::new(Body::from_stream(body));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        "text/csv; charset=utf-8".parse().unwrap(),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        "attachment; filename=\"consent.csv\"".parse().unwrap(),
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(text: &str) -> Vec<String> {
        text.split('|').map(str::to_owned).collect()
    }

    fn keys(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    const PERSON: &str = "0194f2c0-0000-7000-8000-000000000001";

    #[test]
    fn the_header_is_the_four_base_columns_then_dimension_columns() {
        assert_eq!(
            parse_header(&fields("person_id|channel|topic|status")).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            parse_header(&fields(
                " Person_ID |channel|topic|status|dimension:vertical|dimension:brand"
            ))
            .unwrap(),
            keys(&["vertical", "brand"])
        );
        for bad in [
            "person_id|channel|status|topic",
            "person_id|channel|topic",
            "person_id|channel|topic|status|vertical",
            "person_id|channel|topic|status|dimension:topic",
            "person_id|channel|topic|status|dimension:Bad Key",
            "person_id|channel|topic|status|dimension:a|dimension:a",
        ] {
            assert!(
                matches!(parse_header(&fields(bad)), Err(AdminError::Invalid(error)) if error.field() == "header"),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_row_without_dimension_columns_is_an_unscoped_state() {
        let row = parse_row(&fields(&format!("{PERSON}|email||subscribed")), &[]).unwrap();

        assert_eq!(row.person.to_string(), PERSON);
        assert_eq!(row.topic, None);
        assert_eq!(row.status, ConsentStatus::Subscribed);
        assert!(row.dimensions.is_empty());
    }

    #[test]
    fn dimension_cells_scope_a_row_and_an_empty_cell_means_all_values() {
        let columns = keys(&["vertical", "brand"]);
        let both = parse_row(
            &fields(&format!("{PERSON}|email|news|unsubscribed|retail|acme")),
            &columns,
        )
        .unwrap();
        let one = parse_row(
            &fields(&format!("{PERSON}|email|news|unsubscribed|retail|")),
            &columns,
        )
        .unwrap();
        let short = parse_row(
            &fields(&format!("{PERSON}|email|news|unsubscribed|retail")),
            &columns,
        )
        .unwrap();

        assert_eq!(both.dimensions.len(), 2);
        assert_eq!(both.topic.as_deref(), Some("news"));
        assert_eq!(one.dimensions.len(), 1);
        assert_eq!(short, one);
    }

    #[test]
    fn a_bad_row_gets_one_code_and_never_echoes_a_value() {
        let columns = keys(&["vertical"]);
        for (row, code) in [
            ("x|email||subscribed", "invalid_person_id"),
            (&format!("{PERSON}|sms||subscribed"), "invalid_channel"),
            (&format!("{PERSON}|email||unknown"), "invalid_status"),
            (&format!("{PERSON}|email||maybe"), "invalid_status"),
            (&format!("{PERSON}|email|"), "malformed_row"),
            (
                &format!("{PERSON}|email||subscribed|Not A Key|extra"),
                "malformed_row",
            ),
            (
                &format!("{PERSON}|email||subscribed|Not A Key"),
                "invalid_dimension",
            ),
        ] {
            assert_eq!(parse_row(&fields(row), &columns), Err(code), "{row}");
        }
    }

    #[test]
    fn an_exported_line_has_a_cell_per_dimension_column_and_reads_back() {
        let row = ConsentExportRow {
            person_id: PERSON.parse().unwrap(),
            channel: "email".into(),
            topic_key: Some("news".into()),
            dimensions: json!({"vertical": "retail"}),
            status: "unsubscribed".into(),
            dimensions_key: "vertical=retail".into(),
        };
        let columns = keys(&["brand", "vertical"]);

        let line = export_line(&row, &columns);

        assert_eq!(
            export_header(&columns),
            "person_id,channel,topic,status,dimension:brand,dimension:vertical\n"
        );
        assert_eq!(line, format!("{PERSON},email,news,unsubscribed,,retail\n"));
        let parsed = csv::parse(&line).pop().unwrap().fields;
        let back = parse_row(&parsed, &columns).unwrap();
        assert_eq!(
            back.dimensions.get("vertical").map(String::as_str),
            Some("retail")
        );
        assert_eq!(back.status, ConsentStatus::Unsubscribed);
        assert_eq!(export_header(&[]), "person_id,channel,topic,status\n");
    }
}
