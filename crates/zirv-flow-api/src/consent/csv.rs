//! The little CSV the suppression import and export need: RFC 4180 records with quoted
//! fields, `""` as an escaped quote, and LF or CRLF line ends.

use crate::problem::ApiError;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use zirv_flow_core::csv_file::write_field;

/// The largest CSV an import takes.
const MAX_IMPORT_BYTES: usize = 10 * 1024 * 1024;

/// One record and the line of the file it starts on, the first line being 1.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct Record {
    pub(super) line: usize,
    pub(super) fields: Vec<String>,
}

/// Splits `text` into records of fields. A byte order mark at the start is ignored, blank
/// lines are skipped, and a quote that never closes ends the last field at the end of the
/// text. Lines are counted by LF, by CRLF once, and by a CR on its own; a line break inside
/// quotes counts too.
pub(super) fn parse(text: &str) -> Vec<Record> {
    parse_up_to(text, usize::MAX)
}

/// Like [`parse`], but stops reading as soon as it has more than `limit` records, and returns
/// those `limit + 1`. A caller that refuses a file of more than `limit` records learns that
/// without reading the rest of a body it will refuse.
pub(super) fn parse_up_to(text: &str, limit: usize) -> Vec<Record> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut records = Vec::new();
    let mut record: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut in_record = false;
    let mut line = 1;
    let mut start = 1;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let breaks = c == '\n' || (c == '\r' && chars.peek() != Some(&'\n'));
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => quoted = false,
                _ => field.push(c),
            }
            if breaks {
                line += 1;
            }
            continue;
        }
        if breaks {
            if in_record {
                record.push(std::mem::take(&mut field));
                records.push(Record {
                    line: start,
                    fields: std::mem::take(&mut record),
                });
                if records.len() > limit {
                    return records;
                }
            }
            in_record = false;
            line += 1;
            continue;
        }
        if c == '\r' {
            continue;
        }
        if !in_record {
            in_record = true;
            start = line;
        }
        match c {
            '"' if field.is_empty() => quoted = true,
            ',' => record.push(std::mem::take(&mut field)),
            _ => field.push(c),
        }
    }
    if in_record {
        record.push(field);
        records.push(Record {
            line: start,
            fields: record,
        });
    }
    records
}

/// One field of an exported record: quoted when it holds a comma, a quote or a line break.
pub(crate) fn field(value: &str) -> String {
    let mut out = String::new();
    write_field(&mut out, value, ',');
    out
}

/// The text of a `text/csv` request body of at most 10 MiB.
pub(super) async fn read_body(headers: &HeaderMap, body: Body) -> Result<String, ApiError> {
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
    String::from_utf8(bytes.into()).map_err(|_| ApiError::bad_request("the CSV must be UTF-8"))
}

/// A streamed CSV download named `filename`.
pub(super) fn download(body: Body, filename: &str) -> Response {
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        "text/csv; charset=utf-8".parse().unwrap(),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"{filename}\"")
            .parse()
            .unwrap(),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(text: &str) -> Vec<Vec<String>> {
        parse(text)
            .into_iter()
            .map(|record| record.fields)
            .collect()
    }

    fn lines(text: &str) -> Vec<usize> {
        parse(text).into_iter().map(|record| record.line).collect()
    }

    fn record(fields: &[&str]) -> Vec<String> {
        fields.iter().map(|field| (*field).to_owned()).collect()
    }

    #[test]
    fn a_limited_parse_stops_at_the_first_record_past_the_limit() {
        let text = "a\nb\nc\nd\ne\n";

        let stopped = parse_up_to(text, 2);
        let whole = parse_up_to(text, 5);

        assert_eq!(lines_of(&stopped), [1, 2, 3]);
        assert_eq!(whole.len(), 5);
        assert_eq!(parse(text).len(), 5);
        assert_eq!(parse_up_to("a\nb", 2).len(), 2);
    }

    fn lines_of(records: &[Record]) -> Vec<usize> {
        records.iter().map(|record| record.line).collect()
    }

    #[test]
    fn records_split_on_commas_and_either_line_end() {
        assert_eq!(
            rows("kind,value\nemail,a@x.com\r\nphone,+4512345678"),
            [
                record(&["kind", "value"]),
                record(&["email", "a@x.com"]),
                record(&["phone", "+4512345678"])
            ]
        );
    }

    #[test]
    fn quoted_fields_keep_commas_quotes_and_line_breaks() {
        assert_eq!(
            rows("a,\"b,c\",\"say \"\"hi\"\"\",\"x\ny\"\n"),
            [record(&["a", "b,c", "say \"hi\"", "x\ny"])]
        );
    }

    #[test]
    fn blank_lines_and_a_byte_order_mark_are_ignored_and_empty_fields_kept() {
        assert_eq!(
            rows("\u{feff}kind,value,note\n\n\nemail,a@x.com,\n,,\n"),
            [
                record(&["kind", "value", "note"]),
                record(&["email", "a@x.com", ""]),
                record(&["", "", ""])
            ]
        );
    }

    #[test]
    fn an_unclosed_quote_ends_at_the_end_of_the_text() {
        assert_eq!(rows("a,\"b\nc"), [record(&["a", "b\nc"])]);
        assert!(parse("").is_empty());
    }

    #[test]
    fn a_record_reports_the_line_it_starts_on() {
        assert_eq!(lines("a\nb\nc"), [1, 2, 3]);
        assert_eq!(lines("a\n\n\nb"), [1, 4]);
        assert_eq!(lines("\u{feff}a\r\nb\r\n\r\nc"), [1, 2, 4]);
        assert_eq!(lines("a\rb\r\rc"), [1, 2, 4]);
        assert_eq!(lines("a,\"x\ny\nz\"\nb"), [1, 4]);
        assert_eq!(lines("a,\"x\r\ny\"\nb\r"), [1, 3]);
        assert_eq!(lines("a,\"x\ry\"\nb"), [1, 3]);
    }

    #[test]
    fn an_exported_field_is_quoted_only_when_it_needs_to_be() {
        assert_eq!(field("plain"), "plain");
        assert_eq!(field("a,b"), "\"a,b\"");
        assert_eq!(field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(
            rows(&format!("{},x", field("a\"b,c"))),
            [record(&["a\"b,c", "x"])]
        );
    }
}
