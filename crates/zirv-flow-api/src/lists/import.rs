//! Adding people to a list from a CSV of identifiers. The file names people that exist; it
//! never creates one. The answer holds counts, line numbers and reason codes, never values.

use axum::Json;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{Path, RawQuery, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, StatusCode};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::people::Identifier;
use zirv_flow_store::{ListSource, MAX_PEOPLE_PER_CALL};

use super::{capped, membership_error};
use crate::AppState;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::problem::{ApiError, Problem};

/// The largest body the import takes.
pub(crate) const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;
/// The most data rows a file may hold, the header not counted.
const MAX_ROWS: usize = 100_000;
/// Identifiers looked up per query.
const LOOKUP_BATCH: usize = 5_000;

#[derive(Serialize, ToSchema)]
pub(crate) struct UnmatchedRow {
    /// The line of the file the row starts on, counting the header as line 1. A quoted value
    /// that spans lines moves later rows down, as an editor shows them.
    row: usize,
    /// `not_found`, `malformed_row`, or the rule the identifier broke, such as `invalid_email`.
    #[schema(example = "not_found")]
    reason: &'static str,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ImportBody {
    /// Rows that named an existing person.
    matched: usize,
    /// People newly added; on a dry run, the people that would be.
    added: usize,
    /// Matched rows whose person was a member already or appears earlier in the file.
    already_member: usize,
    /// The first 1000 rows that were skipped, in file order.
    unmatched: Vec<UnmatchedRow>,
    /// How many rows were skipped, however many `unmatched` lists.
    unmatched_count: usize,
}

/// Why a file was refused as a whole.
#[derive(Debug, PartialEq)]
enum CsvError {
    NotUtf8,
    UnterminatedQuote,
    TooManyRows,
    BadHeader,
}

impl CsvError {
    fn detail(&self) -> String {
        match self {
            Self::NotUtf8 => "body: must be UTF-8 text".to_owned(),
            Self::UnterminatedQuote => "body: a quoted field is never closed".to_owned(),
            Self::TooManyRows => format!("body: at most {MAX_ROWS} rows"),
            Self::BadHeader => "body: the first row must be the header kind,value".to_owned(),
        }
    }
}

/// Splits CSV text (RFC 4180: quoted fields may hold commas, quotes written twice, and line
/// breaks) into records, each with the line of the file it starts on (the first line is 1),
/// and refuses a file of more than `max` records. Lines that are empty are not records.
fn parse_records(text: &str, max: usize) -> Result<Vec<(usize, Vec<String>)>, CsvError> {
    let mut records: Vec<(usize, Vec<String>)> = Vec::new();
    let mut record: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut was_quoted = false;
    let mut line = 1;
    let mut start: Option<usize> = None;
    let mut chars = text.chars().peekable();
    let mut finish_record = |record: &mut Vec<String>,
                             field: &mut String,
                             was_quoted: &mut bool,
                             start: &mut Option<usize>| {
        let Some(first_line) = start.take() else {
            return Ok(());
        };
        record.push(std::mem::take(field));
        *was_quoted = false;
        if records.len() == max {
            return Err(CsvError::TooManyRows);
        }
        records.push((first_line, std::mem::take(record)));
        Ok(())
    };
    while let Some(c) = chars.next() {
        if start.is_none() && !matches!(c, '\n' | '\r') {
            start = Some(line);
        }
        match c {
            '"' if quoted => {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            }
            '"' if field.is_empty() && !was_quoted => {
                quoted = true;
                was_quoted = true;
            }
            ',' if !quoted => {
                record.push(std::mem::take(&mut field));
                was_quoted = false;
            }
            '\r' if !quoted && chars.peek() == Some(&'\n') => {}
            '\n' | '\r' if !quoted => {
                finish_record(&mut record, &mut field, &mut was_quoted, &mut start)?;
                line += 1;
            }
            _ => {
                field.push(c);
                if c == '\n' || (c == '\r' && chars.peek() != Some(&'\n')) {
                    line += 1;
                }
            }
        }
    }
    if quoted {
        return Err(CsvError::UnterminatedQuote);
    }
    finish_record(&mut record, &mut field, &mut was_quoted, &mut start)?;
    Ok(records)
}

/// The data rows of a file whose header is `kind,value`, each with the line it starts on (the
/// header is line 1). At most [`MAX_ROWS`] data rows.
fn read_rows(body: &[u8]) -> Result<Vec<(usize, Vec<String>)>, CsvError> {
    let text = std::str::from_utf8(body).map_err(|_| CsvError::NotUtf8)?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut records = parse_records(text, MAX_ROWS + 1)?;
    if records.is_empty() {
        return Err(CsvError::BadHeader);
    }
    let (_, header) = records.remove(0);
    let is_header = header.len() == 2
        && header[0].trim().eq_ignore_ascii_case("kind")
        && header[1].trim().eq_ignore_ascii_case("value");
    if !is_header {
        return Err(CsvError::BadHeader);
    }
    Ok(records)
}

fn dry_run(query: &str) -> Result<bool, ApiError> {
    let mut dry = false;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if key == "dry_run" {
            dry = match value.as_ref() {
                "true" => true,
                "false" => false,
                _ => return Err(ApiError::bad_request("dry_run must be true or false")),
            };
        }
    }
    Ok(dry)
}

fn is_csv(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("text/csv"))
}

/// Adds the people a CSV of `kind,value` rows names. With `dry_run=true` nothing is kept and
/// the answer says what a real import would do. Rows no person owns, and rows that are
/// malformed, come back as line numbers (the header is line 1) with a reason code; values are never echoed. At most
/// 10 MiB and 100,000 rows. Membership changes go to the people's history and the decision
/// log, not the audit log.
#[utoipa::path(
    post,
    operation_id = "import_list_members",
    path = "/api/v1/workspaces/{ws}/lists/{id}/members/import",
    tag = "lists",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "List id"),
        ("dry_run" = Option<bool>, Query, description = "Count only; change nothing")
    ),
    request_body(content = String, content_type = "text/csv", description = "A header row `kind,value`, then one identifier per row"),
    responses(
        (status = 200, description = "What the import did, or would do", body = ImportBody),
        (status = 400, description = "dry_run is not true or false", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or list, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The people changed at the same moment; try again", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "The body is over 10 MiB", body = Problem, content_type = "application/problem+json"),
        (status = 415, description = "The content type is not text/csv", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Not UTF-8, a wrong header, an unclosed quote, or more than 100,000 rows", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn import(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<ImportBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Edit).await?;
    let list: Uuid = path_id(&id)?;
    let dry = dry_run(query.as_deref().unwrap_or_default())?;
    if !is_csv(&headers) {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "expected a CSV body (content-type: text/csv)",
        ));
    }
    let body = body.map_err(|rejection| {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "request body is too large")
        } else {
            ApiError::bad_request("request body could not be read")
        }
    })?;
    zirv_flow_store::find_list(&state.pool, workspace.id, list)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let rows = read_rows(&body).map_err(|error| ApiError::unprocessable(error.detail()))?;

    let mut unmatched: Vec<UnmatchedRow> = Vec::new();
    let mut parsed: Vec<(usize, Identifier)> = Vec::with_capacity(rows.len());
    for (row, fields) in &rows {
        match fields.as_slice() {
            [kind, value] => match Identifier::parse(kind.trim(), value) {
                Ok(identifier) => parsed.push((*row, identifier)),
                Err(error) => unmatched.push(UnmatchedRow {
                    row: *row,
                    reason: error.reason(),
                }),
            },
            _ => unmatched.push(UnmatchedRow {
                row: *row,
                reason: "malformed_row",
            }),
        }
    }

    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let mut people: Vec<Uuid> = Vec::with_capacity(parsed.len());
    for chunk in parsed.chunks(LOOKUP_BATCH) {
        let identifiers: Vec<Identifier> = chunk.iter().map(|(_, id)| id.clone()).collect();
        let owners =
            zirv_flow_store::find_people_by_identifiers(&mut *tx, workspace.id, &identifiers)
                .await?;
        for ((row, _), owner) in chunk.iter().zip(owners) {
            match owner {
                Some(person) => people.push(person),
                None => unmatched.push(UnmatchedRow {
                    row: *row,
                    reason: "not_found",
                }),
            }
        }
    }
    unmatched.sort_by_key(|unmatched| unmatched.row);
    let trace = crate::trace::request_trace();
    let mut added = 0;
    for chunk in people.chunks(MAX_PEOPLE_PER_CALL) {
        let outcome = zirv_flow_store::add_members(
            &mut tx,
            workspace.id,
            list,
            chunk,
            ListSource::Import,
            principal.user_id(),
            &trace,
        )
        .await
        .map_err(|error| ApiError::from(membership_error(error)))?;
        added += outcome.changed;
    }
    if dry {
        drop(tx);
    } else {
        zirv_flow_store::commit(tx).await?;
    }
    let (unmatched, unmatched_count) = capped(unmatched);
    Ok(Json(ImportBody {
        matched: people.len(),
        added,
        already_member: people.len() - added,
        unmatched,
        unmatched_count,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(text: &str) -> Vec<(usize, Vec<String>)> {
        read_rows(text.as_bytes()).unwrap()
    }

    #[test]
    fn rows_are_numbered_by_line_with_the_header_as_line_one() {
        let parsed = rows("kind,value\nemail,a@example.com\r\nphone,+4512345678\n");

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0, 2);
        assert_eq!(parsed[0].1, ["email", "a@example.com"]);
        assert_eq!(parsed[1].0, 3);
        assert_eq!(parsed[1].1, ["phone", "+4512345678"]);
    }

    #[test]
    fn quoted_fields_keep_commas_quotes_and_line_breaks() {
        let parsed = rows("kind,value\n\"external:crm\",\"a,\"\"b\"\"\nc\"\nemail,x@example.com");

        assert_eq!(parsed[0].1, ["external:crm", "a,\"b\"\nc"]);
        assert_eq!(parsed[1].1, ["email", "x@example.com"]);
        assert_eq!((parsed[0].0, parsed[1].0), (2, 4));
    }

    #[test]
    fn a_bom_blank_lines_and_a_missing_final_newline_are_tolerated() {
        let parsed = rows("\u{feff}Kind , VALUE\n\nemail,a@example.com\n\n\nphone,+4512345678");

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0, 3);
        assert_eq!(parsed[1].0, 6);
    }

    #[test]
    fn a_quoted_value_over_several_lines_moves_the_later_lines_down() {
        let parsed = rows(
            "kind,value\r\n\"email\",\"a\r\nb\r\nc\"\r\n\r\nphone,+4512345678\r\nemail,x@example.com\r\n",
        );

        let lines: Vec<usize> = parsed.iter().map(|(line, _)| *line).collect();
        assert_eq!(lines, [2, 6, 7]);
        assert_eq!(parsed[0].1[1], "a\r\nb\r\nc");
    }

    #[test]
    fn a_lone_carriage_return_counts_as_a_line_break() {
        let parsed = rows("kind,value\remail,a@example.com\rphone,+4512345678");

        let lines: Vec<usize> = parsed.iter().map(|(line, _)| *line).collect();
        assert_eq!(lines, [2, 3]);
    }

    #[test]
    fn a_wrong_or_missing_header_is_refused() {
        for bad in [
            "",
            "email,a@example.com\n",
            "kind\n",
            "kind,value,extra\n",
            "value,kind\n",
        ] {
            assert_eq!(
                read_rows(bad.as_bytes()),
                Err(CsvError::BadHeader),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn invalid_utf8_and_an_unclosed_quote_are_refused() {
        assert_eq!(read_rows(&[0xff, 0xfe]), Err(CsvError::NotUtf8));
        assert_eq!(
            read_rows(b"kind,value\nemail,\"a@example.com\n"),
            Err(CsvError::UnterminatedQuote)
        );
    }

    #[test]
    fn a_file_may_hold_exactly_the_most_rows() {
        let mut text = String::from("kind,value\n");
        for _ in 0..MAX_ROWS {
            text.push_str("email,a@example.com\n");
        }
        assert_eq!(read_rows(text.as_bytes()).unwrap().len(), MAX_ROWS);

        text.push_str("email,b@example.com\n");

        assert_eq!(read_rows(text.as_bytes()), Err(CsvError::TooManyRows));
    }

    #[test]
    fn a_row_with_the_wrong_number_of_fields_is_kept_for_the_caller_to_report() {
        let parsed = rows("kind,value\nemail\nemail,a@example.com,extra\n");

        assert_eq!(parsed[0].1.len(), 1);
        assert_eq!(parsed[1].1.len(), 3);
    }

    #[test]
    fn dry_run_is_a_boolean_and_off_by_default() {
        assert!(!dry_run("").unwrap());
        assert!(dry_run("dry_run=true").unwrap());
        assert!(!dry_run("dry_run=false").unwrap());
        assert_eq!(
            dry_run("dry_run=yes").unwrap_err().status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn only_text_csv_is_accepted_whatever_its_parameters() {
        let with = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_TYPE, value.parse().unwrap());
            headers
        };

        assert!(is_csv(&with("text/csv")));
        assert!(is_csv(&with("Text/CSV; charset=utf-8")));
        assert!(!is_csv(&with("application/json")));
        assert!(!is_csv(&HeaderMap::new()));
    }

    #[test]
    fn an_error_detail_names_the_rule_and_no_value() {
        assert!(CsvError::BadHeader.detail().contains("kind,value"));
        assert!(CsvError::TooManyRows.detail().contains("100000"));
    }
}
