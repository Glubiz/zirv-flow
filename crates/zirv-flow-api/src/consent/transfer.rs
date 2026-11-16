//! Consent import and export as CSV. A row is one state of one person: the person id, the
//! channel, the topic key (empty for the channel as a whole), the status, and then one
//! `dimension:<key>` column per dimension the file scopes by, holding the value key (empty
//! for all values). A file with only the first four columns, the shape that predates
//! dimensions, still imports and means every row is unscoped.

use std::io;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, RawQuery, State};
use axum::http::HeaderMap;
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
use super::suppressions::EXPORT_PAGE;
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
    /// Rows that repeated the status already held, or were older than the state they scope.
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

/// The header's dimension keys and the data rows. Reading stops at the first row past the cap,
/// so an oversized body is refused without being parsed to its end.
fn read_records(text: &str) -> Result<(Vec<String>, Vec<csv::Record>), AdminError> {
    let mut records = csv::parse_up_to(text, MAX_IMPORT_ROWS + 1).into_iter();
    let header = records
        .next()
        .map(|record| record.fields)
        .unwrap_or_default();
    let dimension_keys = parse_header(&header)?;
    let records: Vec<csv::Record> = records.collect();
    if records.len() > MAX_IMPORT_ROWS {
        return Err(invalid("rows", "must be at most 10000 rows"));
    }
    Ok((dimension_keys, records))
}

/// Imports a CSV of consent states, one write per row, in one transaction. On a dry run
/// nothing is kept. Bad rows are reported by line number and code and do not stop the rest.
/// A real import is audited as `consent.imported` with counts only. Each written change is
/// recorded in the person history with the source `import`.
///
/// Every row is read first, then the valid ones are written in order of the person each write
/// locks (a person merged into another is locked as the survivor, so the rows are ordered by
/// survivor, resolved on each attempt; one person's rows keep their file order, so the later one still wins). A write
/// holds its person's lock to the end of the transaction, so two imports that write the same
/// people in one order cannot deadlock each other, whichever merged ids their files name; an
/// import that still loses a deadlock is run once more, dry run or not, with the survivors
/// resolved and the rows sorted again inside that attempt, so a merge that landed meanwhile is
/// seen. That is a bounded backstop, not a guarantee: a second deadlock is returned as the
/// retryable error. A dry run writes too, to report what a commit would do, and holds its locks
/// until it is rolled back; it keeps nothing. A state scoped to an archived
/// dimension or value is accepted, as an export lists those; one scoped to a dimension or value
/// that does not exist is not.
pub(crate) async fn import(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    text: &str,
    dry_run: bool,
    actor: Option<Uuid>,
) -> Result<ConsentImportBody, AdminError> {
    let (dimension_keys, records) = read_records(text)?;

    let topics: HashMap<String, Uuid> = zirv_flow_store::list_topics(pool, workspace, true)
        .await?
        .into_iter()
        .map(|topic| (topic.key, topic.id))
        .collect();
    let mut body = ConsentImportBody {
        dry_run,
        changed: 0,
        unchanged: 0,
        invalid: Vec::new(),
    };
    let mut rows: Vec<(usize, Row, Option<Uuid>)> = Vec::with_capacity(records.len());
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
        rows.push((line, row, topic));
    }
    // A deadlock that survives the order is run once more, from a fresh order: a merge that
    // landed after the first resolution is seen by the second.
    let mut attempt = sorted_write(
        pool,
        context,
        workspace,
        &rows,
        body.invalid.len(),
        dry_run,
        actor,
    )
    .await;
    if matches!(&attempt, Err(AdminError::Store(error)) if error.is_retryable()) {
        attempt = sorted_write(
            pool,
            context,
            workspace,
            &rows,
            body.invalid.len(),
            dry_run,
            actor,
        )
        .await;
    }
    let written = attempt?;
    body.changed = written.changed;
    body.unchanged = written.unchanged;
    body.invalid.extend(written.invalid);
    body.invalid.sort_by_key(|row| row.row);
    Ok(body)
}

/// One attempt: resolves each row's survivor now, orders the rows by (survivor, line) and writes
/// them. A person that does not resolve keeps its own id and is reported by the write. The sort
/// is on (survivor, line), so one real person's rows stay in file order whichever alias they
/// name, and every attempt takes its locks in the same order.
async fn sorted_write(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    rows: &[(usize, Row, Option<Uuid>)],
    already_invalid: usize,
    dry_run: bool,
    actor: Option<Uuid>,
) -> Result<Written, AdminError> {
    let mut survivors: HashMap<Uuid, Uuid> = HashMap::new();
    for (_, row, _) in rows {
        if !survivors.contains_key(&row.person) {
            let survivor = zirv_flow_store::current_person(pool, workspace, row.person)
                .await?
                .unwrap_or(row.person);
            survivors.insert(row.person, survivor);
        }
    }
    let mut sorted: Vec<&(usize, Row, Option<Uuid>)> = rows.iter().collect();
    sorted.sort_by_key(|(line, row, _)| (survivors[&row.person], *line));
    write_rows(
        pool,
        context,
        workspace,
        &sorted,
        already_invalid,
        dry_run,
        actor,
    )
    .await
}

/// What one attempt at writing the valid rows did.
struct Written {
    changed: usize,
    unchanged: usize,
    invalid: Vec<ConsentImportRowBody>,
}

/// Writes `rows` in one transaction, in the order given, and commits it unless this is a dry
/// run. `already_invalid` counts the rows refused before, for the audit entry.
async fn write_rows(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    rows: &[&(usize, Row, Option<Uuid>)],
    already_invalid: usize,
    dry_run: bool,
    actor: Option<Uuid>,
) -> Result<Written, AdminError> {
    let trace = crate::trace::request_trace();
    let mut tx = zirv_flow_store::begin(pool).await?;
    let mut written = Written {
        changed: 0,
        unchanged: 0,
        invalid: Vec::new(),
    };
    for (line, row, topic) in rows.iter().copied() {
        let outcome = zirv_flow_store::set_consent(
            &mut tx,
            workspace,
            &ConsentChange {
                person: row.person,
                channel: row.channel,
                topic: *topic,
                dimensions: row.dimensions.clone(),
                status: row.status,
                source: ChangeSource::Import,
                evidence: json!({}),
                occurred_at: OffsetDateTime::now_utc(),
                actor,
            },
            &trace,
        )
        .await?;
        let reason = match outcome {
            ConsentOutcome::Changed { .. } => {
                written.changed += 1;
                continue;
            }
            ConsentOutcome::Unchanged | ConsentOutcome::Superseded => {
                written.unchanged += 1;
                continue;
            }
            ConsentOutcome::NoSuchPerson => "no_such_person",
            ConsentOutcome::NoSuchTopic => "unknown_topic",
            ConsentOutcome::NoSuchDimension => "unknown_dimension",
            ConsentOutcome::NoSuchDimensionValue => "unknown_dimension_value",
        };
        written.invalid.push(ConsentImportRowBody {
            row: *line,
            reason: reason.to_owned(),
        });
    }
    if !dry_run {
        admin::audit(
            &mut tx,
            context,
            Some(workspace),
            "consent.imported",
            None,
            json!({
                "changed": written.changed,
                "unchanged": written.unchanged,
                "invalid": already_invalid + written.invalid.len(),
            }),
        )
        .await?;
        zirv_flow_store::commit(tx).await?;
    }
    Ok(written)
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
    let text = csv::read_body(&headers, body).await?;
    let (_, actor) = origin(&principal);
    let result = import(
        &state.pool,
        &principal.context(),
        workspace.id,
        &text,
        dry_run,
        actor,
    )
    .await?;
    Ok(Json(result))
}

/// Every consent state as CSV, `person_id,channel,topic,status` and one `dimension:<key>`
/// column per declared dimension, streamed page by page. The file imports back as it is, a
/// state scoped to an archived dimension or value included; one that names a dimension or value
/// the workspace never had is refused.
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
    Ok(csv::download(Body::from_stream(body), "consent.csv"))
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
    fn an_import_of_more_than_the_cap_is_refused_and_the_parser_stops_at_the_first_row_past_it() {
        let header = "person_id,channel,topic,status\n";
        let row = format!("{PERSON},email,,subscribed\n");
        let at_cap = format!("{header}{}", row.repeat(MAX_IMPORT_ROWS));
        let over = format!("{header}{}", row.repeat(MAX_IMPORT_ROWS + 1));
        let way_over = format!("{header}{}", row.repeat(MAX_IMPORT_ROWS * 3));

        let (_, rows) = read_records(&at_cap).unwrap();
        let refused = read_records(&over);
        let stopped = csv::parse_up_to(&way_over, MAX_IMPORT_ROWS + 1);

        assert_eq!(rows.len(), MAX_IMPORT_ROWS);
        assert!(matches!(refused, Err(AdminError::Invalid(error)) if error.field() == "rows"));
        assert!(read_records(&way_over).is_err());
        assert_eq!(stopped.len(), MAX_IMPORT_ROWS + 2);
        assert_eq!(
            stopped.last().map(|record| record.line),
            Some(MAX_IMPORT_ROWS + 2)
        );
    }

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
            topic_id: Some(Uuid::nil()),
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

    async fn workspace_with_people(pool: &PgPool, people: u32) -> (WorkspaceId, Vec<Uuid>) {
        use zirv_flow_core::{Slug, WorkspaceName};

        zirv_flow_store::migrate(pool).await.unwrap();
        let ws = zirv_flow_store::create_workspace(
            pool,
            &WorkspaceName::parse("ours").unwrap(),
            &Slug::parse("ours").unwrap(),
        )
        .await
        .unwrap()
        .id;
        let ids = sqlx::query_scalar(
            "INSERT INTO people (id, workspace_id) \
             SELECT gen_random_uuid(), $1 FROM generate_series(1, $2) RETURNING id",
        )
        .bind(ws.as_uuid())
        .bind(people as i32)
        .fetch_all(pool)
        .await
        .unwrap();
        (ws, ids)
    }

    const HEADER: &str = "person_id,channel,topic,status\n";

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn two_concurrent_imports_touching_the_same_people_in_opposite_order_both_succeed(
        pool: PgPool,
    ) {
        let (ws, ids) = workspace_with_people(&pool, 602).await;
        let (a, b) = (ids[0], ids[1]);
        let line = |id: &Uuid| format!("{id},email,,subscribed\n");
        let mut one = vec![a];
        one.extend(&ids[2..302]);
        one.push(b);
        let mut two = vec![b];
        two.extend(&ids[302..602]);
        two.push(a);
        let one = format!("{HEADER}{}", one.iter().map(&line).collect::<String>());
        let two = format!("{HEADER}{}", two.iter().map(&line).collect::<String>());
        let context = Context::system();

        let (first, second) = tokio::join!(
            import(&pool, &context, ws, &one, false, None),
            import(&pool, &context, ws, &two, false, None)
        );

        assert!(
            first.is_ok() && second.is_ok(),
            "first: {:?}, second: {:?}",
            first.as_ref().err(),
            second.as_ref().err()
        );
    }

    /// A trigger on `consent_changes` that raises a deadlock (`40P01`) the first time it fires,
    /// whatever the transaction: a sequence is not rolled back, so the next attempt passes.
    async fn deadlock_once_on_the_first_history_row(pool: &PgPool) {
        for statement in [
            "CREATE SEQUENCE one_shot_deadlock",
            "CREATE FUNCTION one_shot_deadlock() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN \
                 IF nextval('one_shot_deadlock') = 1 THEN \
                     RAISE EXCEPTION 'deadlock detected' USING ERRCODE = '40P01'; \
                 END IF; \
                 RETURN NEW; \
             END $$",
            "CREATE TRIGGER one_shot_deadlock BEFORE INSERT ON consent_changes \
             FOR EACH ROW EXECUTE FUNCTION one_shot_deadlock()",
        ] {
            sqlx::query(statement).execute(pool).await.unwrap();
        }
    }

    async fn count(pool: &PgPool, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_dry_run_that_loses_a_deadlock_is_retried_once_and_keeps_nothing(pool: PgPool) {
        let (ws, ids) = workspace_with_people(&pool, 3).await;
        let file = format!(
            "{HEADER}{}",
            ids.iter()
                .map(|id| format!(
                    "{id},email,,subscribed
"
                ))
                .collect::<String>()
        );
        let expected = import(&pool, &Context::system(), ws, &file, true, None)
            .await
            .unwrap();
        deadlock_once_on_the_first_history_row(&pool).await;

        let report = import(&pool, &Context::system(), ws, &file, true, None)
            .await
            .unwrap();

        assert_eq!(
            (report.changed, report.unchanged, report.invalid.len()),
            (expected.changed, expected.unchanged, expected.invalid.len())
        );
        assert_eq!(report.changed, 3);
        assert_eq!(count(&pool, "SELECT count(*) FROM consent").await, 0);
        assert_eq!(
            count(&pool, "SELECT count(*) FROM consent_changes").await,
            0
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_real_import_that_loses_a_deadlock_is_retried_once_and_writes_once(pool: PgPool) {
        let (ws, ids) = workspace_with_people(&pool, 3).await;
        let file = format!(
            "{HEADER}{}",
            ids.iter()
                .map(|id| format!(
                    "{id},email,,subscribed
"
                ))
                .collect::<String>()
        );
        deadlock_once_on_the_first_history_row(&pool).await;

        let report = import(&pool, &Context::system(), ws, &file, false, None)
            .await
            .unwrap();

        assert_eq!((report.changed, report.unchanged), (3, 0));
        assert_eq!(count(&pool, "SELECT count(*) FROM consent").await, 3);
        assert_eq!(
            count(&pool, "SELECT count(*) FROM consent_changes").await,
            3
        );
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM audit_log WHERE action = 'consent.imported'"
            )
            .await,
            1
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn one_persons_rows_apply_in_file_order_and_the_invalid_rows_stay_in_row_order(
        pool: PgPool,
    ) {
        let (ws, ids) = workspace_with_people(&pool, 2).await;
        let (early, late) = (ids[0].min(ids[1]), ids[0].max(ids[1]));
        let stranger = Uuid::now_v7();
        let file = format!(
            "{HEADER}\
             {stranger},email,,subscribed\n\
             {late},email,,subscribed\n\
             not-a-person,email,,subscribed\n\
             {early},email,,subscribed\n\
             {late},email,,unsubscribed\n\
             {early},email,nosuchtopic,subscribed\n\
             {stranger},email,,unsubscribed\n"
        );

        let result = import(&pool, &Context::system(), ws, &file, false, None)
            .await
            .unwrap();

        let rows: Vec<(usize, &str)> = result
            .invalid
            .iter()
            .map(|row| (row.row, row.reason.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                (2, "no_such_person"),
                (4, "invalid_person_id"),
                (7, "unknown_topic"),
                (8, "no_such_person")
            ]
        );
        let status: String = sqlx::query_scalar(
            "SELECT status FROM consent WHERE person_id = $1 AND channel = 'email'",
        )
        .bind(late)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "unsubscribed", "the later row of one person wins");
        assert_eq!((result.changed, result.unchanged), (3, 0));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_export_with_a_value_scoped_state_imports_back_after_the_value_is_archived(
        pool: PgPool,
    ) {
        use zirv_flow_store::{DimensionUpdate, NewDimension};

        let (ws, ids) = workspace_with_people(&pool, 1).await;
        zirv_flow_store::create_dimension(
            &pool,
            ws,
            &NewDimension {
                key: "brand".into(),
                name: "Brand".into(),
            },
        )
        .await
        .unwrap();
        zirv_flow_store::create_dimension_value(&pool, ws, "brand", "acme", "Acme")
            .await
            .unwrap();
        let file = format!("{HEADER_WITH_BRAND}{},email,,unsubscribed,acme\n", ids[0]);
        let stored = import(&pool, &Context::system(), ws, &file, false, None)
            .await
            .unwrap();
        assert!(stored.invalid.is_empty(), "{:?}", stored.invalid.len());
        zirv_flow_store::update_dimension_value(
            &pool,
            ws,
            "brand",
            "acme",
            &DimensionUpdate {
                name: None,
                archived: Some(true),
            },
        )
        .await
        .unwrap();
        let mut connection = zirv_flow_store::begin(&pool).await.unwrap();
        let keys: Vec<String> = zirv_flow_store::list_dimensions(&mut connection, ws, true)
            .await
            .unwrap()
            .into_iter()
            .filter(|dimension| !dimension.builtin)
            .map(|dimension| dimension.key)
            .collect();
        drop(connection);
        let page = zirv_flow_store::consent_export_page(&pool, ws, None, 100)
            .await
            .unwrap();
        let exported = format!(
            "{}{}",
            export_header(&keys),
            page.iter()
                .map(|row| export_line(row, &keys))
                .collect::<String>()
        );

        let result = import(&pool, &Context::system(), ws, &exported, true, None)
            .await
            .unwrap();

        let reasons: Vec<&str> = result
            .invalid
            .iter()
            .map(|row| row.reason.as_str())
            .collect();
        assert!(
            result.invalid.is_empty(),
            "file:\n{exported}\nimport rejected rows: {reasons:?}"
        );
    }

    const HEADER_WITH_BRAND: &str = "person_id,channel,topic,status,dimension:brand\n";

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_import_still_refuses_a_dimension_or_value_that_does_not_exist(pool: PgPool) {
        let (ws, ids) = workspace_with_people(&pool, 1).await;
        zirv_flow_store::create_dimension(
            &pool,
            ws,
            &zirv_flow_store::NewDimension {
                key: "brand".into(),
                name: "Brand".into(),
            },
        )
        .await
        .unwrap();
        let file = format!(
            "person_id,channel,topic,status,dimension:brand,dimension:region\n\
             {id},email,,unsubscribed,acme,\n\
             {id},email,,unsubscribed,,north\n",
            id = ids[0]
        );

        let result = import(&pool, &Context::system(), ws, &file, true, None)
            .await
            .unwrap();

        let reasons: Vec<(usize, &str)> = result
            .invalid
            .iter()
            .map(|row| (row.row, row.reason.as_str()))
            .collect();
        assert_eq!(
            reasons,
            [(2, "unknown_dimension_value"), (3, "unknown_dimension")]
        );
    }
}
