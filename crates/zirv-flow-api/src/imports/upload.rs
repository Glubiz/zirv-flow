//! Uploading a CSV file. The body is streamed, never held whole: it goes into the database in
//! 1 MiB chunks while its SHA-256, its row count, its header and its first rows are taken.
//! The chunks and the import row commit together, so a refused or interrupted upload leaves
//! nothing behind.

use axum::Json;
use axum::extract::{Path, RawQuery, Request, State};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use futures_util::StreamExt;
use serde_json::json;
use zirv_flow_core::csv_file::{CsvError, CsvParser, detect_delimiter};
use zirv_flow_core::mapping::{Header, HeaderError};
use zirv_flow_store::{ImportUpload, UploadScan, UploadStart};

use super::{CsvImportDetailBody, ImportLimits, SAMPLE_ROWS};
use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, may_view_pii, path_id};
use crate::problem::{ApiError, Problem};
use crate::trace::request_trace;

const MAX_NAME_CHARS: usize = 128;
const MAX_FILE_NAME_CHARS: usize = 255;

/// How much of the start of the file is read before its delimiter is judged.
const DETECT_BYTES: usize = 8 * 1024;

/// What the query string says about the upload.
#[derive(Debug, Eq, PartialEq)]
struct UploadParams {
    name: String,
    file_name: String,
    source: String,
    stream: String,
    delimiter: Option<u8>,
}

fn valid_code(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

/// A stream name made from a file name: no extension, lowercase, runs of other characters
/// become one `-`.
fn stream_from(file_name: &str) -> String {
    let stem = file_name
        .rsplit_once('.')
        .map_or(file_name, |(stem, _)| stem);
    let mut stream = String::new();
    for c in stem.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' {
            stream.push(c);
        } else if !stream.ends_with('-') {
            stream.push('-');
        }
    }
    let stream = stream.trim_matches('-');
    let stream: String = stream.chars().take(64).collect();
    let stream = stream.trim_end_matches('-');
    if stream.is_empty() {
        "import".to_owned()
    } else {
        stream.to_owned()
    }
}

fn parse_delimiter(value: &str) -> Result<u8, ApiError> {
    match value {
        "," | "comma" => Ok(b','),
        ";" | "semicolon" => Ok(b';'),
        "\t" | "tab" => Ok(b'\t'),
        _ => Err(ApiError::bad_request(
            "delimiter must be comma, semicolon or tab",
        )),
    }
}

impl UploadParams {
    fn parse(query: &str) -> Result<Self, ApiError> {
        let (mut name, mut file_name, mut source, mut stream, mut delimiter) =
            (None, None, None, None, None);
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            match key.as_ref() {
                "name" => name = Some(value.trim().to_owned()),
                "file_name" => file_name = Some(value.into_owned()),
                "source" => source = Some(value.into_owned()),
                "stream" => stream = Some(value.into_owned()),
                "delimiter" => delimiter = Some(parse_delimiter(&value)?),
                _ => {}
            }
        }
        let name = name.ok_or_else(|| ApiError::bad_request("name is required"))?;
        let length = name.chars().count();
        if length == 0 || length > MAX_NAME_CHARS || name.chars().any(char::is_control) {
            return Err(ApiError::unprocessable(
                "name: must be 1 to 128 characters without control characters",
            ));
        }
        let file_name = file_name.unwrap_or_else(|| name.clone());
        if file_name.chars().count() > MAX_FILE_NAME_CHARS
            || file_name.chars().any(char::is_control)
        {
            return Err(ApiError::unprocessable(
                "file_name: must be at most 255 characters without control characters",
            ));
        }
        let source = source.unwrap_or_else(|| "csv".to_owned());
        if !valid_code(&source) {
            return Err(ApiError::unprocessable(
                "source: must be 1 to 64 characters of a-z, 0-9, _ and -",
            ));
        }
        let stream = stream.unwrap_or_else(|| stream_from(&file_name));
        if !valid_code(&stream) {
            return Err(ApiError::unprocessable(
                "stream: must be 1 to 64 characters of a-z, 0-9, _ and -",
            ));
        }
        Ok(Self {
            name,
            file_name,
            source,
            stream,
            delimiter,
        })
    }
}

/// What the stream of bytes turned out to hold.
struct Scan {
    delimiter: u8,
    header: Option<Vec<String>>,
    rows: i64,
    samples: Vec<Vec<String>>,
}

/// Takes the header, the row count and the first rows from a file fed in pieces.
struct FileScan {
    given: Option<u8>,
    head: Vec<u8>,
    parser: Option<CsvParser>,
    delimiter: u8,
    header: Option<Vec<String>>,
    rows: i64,
    samples: Vec<Vec<String>>,
}

impl FileScan {
    fn new(delimiter: Option<u8>) -> Self {
        Self {
            given: delimiter,
            head: Vec::new(),
            parser: None,
            delimiter: delimiter.unwrap_or(b','),
            header: None,
            rows: 0,
            samples: Vec::new(),
        }
    }

    fn take(&mut self, record: Vec<String>) {
        if self.header.is_none() {
            self.header = Some(record);
            return;
        }
        self.rows += 1;
        if self.samples.len() < SAMPLE_ROWS {
            self.samples.push(record);
        }
    }

    /// Judges the delimiter from the start of the file and feeds that start to the parser.
    fn begin(&mut self) -> Result<(), CsvError> {
        self.delimiter = self.given.unwrap_or_else(|| detect_delimiter(&self.head));
        let mut parser = CsvParser::new(self.delimiter);
        let head = std::mem::take(&mut self.head);
        let mut records = Vec::new();
        parser.push(&head, &mut |_, record| records.push(record))?;
        for record in records {
            self.take(record);
        }
        self.parser = Some(parser);
        Ok(())
    }

    fn push(&mut self, bytes: &[u8]) -> Result<(), CsvError> {
        if self.parser.is_none() {
            self.head.extend_from_slice(bytes);
            if self.head.len() < DETECT_BYTES {
                return Ok(());
            }
            return self.begin();
        }
        let mut records = Vec::new();
        if let Some(parser) = self.parser.as_mut() {
            parser.push(bytes, &mut |_, record| records.push(record))?;
        }
        for record in records {
            self.take(record);
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Scan, CsvError> {
        if self.parser.is_none() {
            self.begin()?;
        }
        let mut records = Vec::new();
        if let Some(parser) = self.parser.as_mut() {
            parser.finish(&mut |_, record| records.push(record))?;
        }
        for record in records {
            self.take(record);
        }
        Ok(Scan {
            delimiter: self.delimiter,
            header: self.header,
            rows: self.rows,
            samples: self.samples,
        })
    }
}

fn csv_problem(error: CsvError) -> ApiError {
    ApiError::unprocessable(format!("body: {error} ({})", error.code()))
}

fn header_problem(error: HeaderError) -> ApiError {
    ApiError::unprocessable(match error {
        HeaderError::Blank => "body: header names must not be blank",
        HeaderError::Duplicate => "body: header names must be unique",
    })
}

fn too_large() -> ApiError {
    ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "the file is too large")
}

fn is_csv(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("text/csv"))
}

/// Reads the body into `upload` and the scan, refusing a file that is too large, stalls for
/// `idle`, breaks off or is not a usable CSV. The caller deletes the upload on an error.
async fn receive(
    upload: &mut ImportUpload,
    body: axum::body::Body,
    limits: ImportLimits,
    delimiter: Option<u8>,
) -> Result<(Scan, Vec<String>, Vec<Vec<String>>), ApiError> {
    let mut scan = FileScan::new(delimiter);
    let mut stream = body.into_data_stream();
    let mut total = 0_u64;
    loop {
        let next = tokio::time::timeout(limits.idle_timeout, stream.next())
            .await
            .map_err(|_| {
                ApiError::new(
                    StatusCode::REQUEST_TIMEOUT,
                    "no data arrived for too long; the upload was abandoned",
                )
            })?;
        let Some(piece) = next else { break };
        let piece = piece.map_err(|_| ApiError::bad_request("the upload was interrupted"))?;
        total += piece.len() as u64;
        if total > limits.max_file_bytes {
            return Err(too_large());
        }
        upload.write(&piece).await.map_err(|error| {
            if error.is_erased() {
                ApiError::new(
                    StatusCode::CONFLICT,
                    "the import was erased while it was being uploaded",
                )
            } else {
                error.into()
            }
        })?;
        scan.push(&piece).map_err(csv_problem)?;
    }
    let mut scanned = scan.finish().map_err(csv_problem)?;
    let names = scanned
        .header
        .take()
        .ok_or_else(|| ApiError::unprocessable("body: the file is empty"))?;
    Header::new(names.clone()).map_err(header_problem)?;
    let samples = std::mem::take(&mut scanned.samples);
    Ok((scanned, names, samples))
}

/// Uploads a CSV file as a new import. The body is the raw `text/csv` file, streamed into the
/// database in 1 MiB chunks; it must be UTF-8 (a byte order mark is ignored) with a header row
/// of unique names. The delimiter is detected from `,`, `;` and tab unless one is given. The
/// answer holds the import, its header and the first 20 rows, masked for callers who may not
/// see values. The import exists as `uploading` (hidden from the history) while the body
/// arrives, each 1 MiB chunk is stored on its own, and an upload that is refused, breaks off
/// or stalls for 60 seconds is deleted with its chunks. Audited as `import.created`.
#[utoipa::path(
    post,
    operation_id = "upload_import",
    path = "/api/v1/workspaces/{ws}/imports",
    tag = "imports",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("name" = String, Query, description = "A name for the import, 1 to 128 characters"),
        ("file_name" = Option<String>, Query, description = "The file's name; defaults to name"),
        ("source" = Option<String>, Query, description = "The source records are written under: a-z, 0-9, _ and -; default csv"),
        ("stream" = Option<String>, Query, description = "The stream records are written under; default the file name without its extension"),
        ("delimiter" = Option<String>, Query, description = "comma, semicolon or tab; detected when absent")
    ),
    request_body(content = String, content_type = "text/csv", description = "The CSV file"),
    responses(
        (status = 201, description = "The new import with its header and sample rows", body = CsvImportDetailBody),
        (status = 400, description = "A malformed query, or an upload that broke off", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 408, description = "No byte arrived for 60 seconds; the upload was abandoned and nothing was kept", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "The file is over imports.max_file_bytes", body = Problem, content_type = "application/problem+json"),
        (status = 415, description = "The content type is not text/csv", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Not UTF-8, empty, or a header with blank or repeated names", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn upload(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    RawQuery(query): RawQuery,
    request: Request,
) -> Result<(StatusCode, Json<CsvImportDetailBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Edit).await?;
    let params = UploadParams::parse(&query.unwrap_or_default())?;
    if !is_csv(request.headers()) {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "expected a CSV body (content-type: text/csv)",
        ));
    }
    let limits = state.imports;
    let declared = request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if declared.is_some_and(|length| length > limits.max_file_bytes) {
        return Err(too_large());
    }

    let trace = request_trace();
    let mut upload = ImportUpload::begin(
        &state.pool,
        workspace.id,
        &UploadStart {
            name: &params.name,
            source: &params.source,
            stream: &params.stream,
            file_name: &params.file_name,
            created_by: principal.user_id(),
            trace: &trace,
        },
    )
    .await?;
    let received = receive(&mut upload, request.into_body(), limits, params.delimiter).await;
    let (scanned, names, samples) = match received {
        Ok(received) => received,
        Err(error) => {
            upload.abort().await;
            return Err(error);
        }
    };
    let (import, mut tx) = upload
        .finish(&UploadScan {
            delimiter: char::from(scanned.delimiter),
            header: &names,
            rows_total: scanned.rows,
        })
        .await?;
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "import.created",
        Some(("import", import.id)),
        json!({ "rows": import.rows_total, "bytes": import.size_bytes }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    let may_see = may_view_pii(&state, &principal, workspace.id).await;
    Ok((
        StatusCode::CREATED,
        Json(CsvImportDetailBody::new(&import, samples, may_see)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(pieces: &[&[u8]], delimiter: Option<u8>) -> Result<Scan, CsvError> {
        let mut scan = FileScan::new(delimiter);
        for piece in pieces {
            scan.push(piece)?;
        }
        scan.finish()
    }

    #[test]
    fn a_scan_takes_the_header_the_row_count_and_the_first_rows() {
        let mut text = String::from("a,b\n");
        for n in 0..30 {
            text.push_str(&format!("{n},x\n"));
        }

        let scanned = scan(&[text.as_bytes()], None).unwrap();

        assert_eq!(scanned.header, Some(vec!["a".to_owned(), "b".to_owned()]));
        assert_eq!(scanned.rows, 30);
        assert_eq!(scanned.samples.len(), SAMPLE_ROWS);
        assert_eq!(scanned.samples[0], ["0", "x"]);
        assert_eq!(scanned.delimiter, b',');
    }

    #[test]
    fn the_delimiter_is_detected_and_a_given_one_wins() {
        assert_eq!(scan(&[b"a;b\n1;2\n"], None).unwrap().delimiter, b';');
        assert_eq!(scan(&[b"a\tb\n1\t2\n"], None).unwrap().delimiter, b'\t');
        let forced = scan(&[b"a;b\n1;2\n"], Some(b',')).unwrap();
        assert_eq!(forced.header, Some(vec!["a;b".to_owned()]));
    }

    #[test]
    fn a_scan_does_not_depend_on_how_the_body_is_cut() {
        let text = b"\xEF\xBB\xBFname;n\n\"a;\nb\";1\nc;2\n";
        let whole = scan(&[text], None).unwrap();
        for cut in 1..text.len() {
            let split = scan(&[&text[..cut], &text[cut..]], None).unwrap();

            assert_eq!(split.header, whole.header, "cut at {cut}");
            assert_eq!(split.rows, 2, "cut at {cut}");
            assert_eq!(split.samples, whole.samples, "cut at {cut}");
            assert_eq!(split.delimiter, b';', "cut at {cut}");
        }
    }

    #[test]
    fn a_body_over_the_detection_window_is_scanned_to_the_end() {
        let mut text = String::from("a,b\n");
        while text.len() < 3 * DETECT_BYTES {
            text.push_str("1,2\n");
        }
        let pieces: Vec<&[u8]> = text.as_bytes().chunks(1000).collect();

        let scanned = scan(&pieces, None).unwrap();

        assert_eq!(
            scanned.rows,
            i64::try_from(text.lines().count() - 1).unwrap()
        );
    }

    #[test]
    fn a_file_that_is_not_utf8_is_refused() {
        assert_eq!(scan(&[b"a\n\xFF\n"], None).err(), Some(CsvError::NotUtf8));
    }

    #[test]
    fn an_empty_file_has_no_header() {
        assert_eq!(scan(&[], None).unwrap().header, None);
        assert_eq!(scan(&[b"\n\n"], None).unwrap().header, None);
    }

    #[test]
    fn the_stream_defaults_to_the_file_name_without_its_extension() {
        for (file, stream) in [
            ("Customers May.csv", "customers-may"),
            ("a_b-c.tsv", "a_b-c"),
            ("archive.2026.csv", "archive-2026"),
            ("--.csv", "import"),
            ("Åre.csv", "re"),
            ("noextension", "noextension"),
        ] {
            assert_eq!(stream_from(file), stream, "{file}");
        }
        assert_eq!(stream_from(&format!("{}.csv", "x".repeat(100))).len(), 64);
    }

    #[test]
    fn the_query_names_the_import_and_defaults_the_rest() {
        let params = UploadParams::parse("name=May%20list&file_name=may.csv").unwrap();

        assert_eq!(
            params,
            UploadParams {
                name: "May list".into(),
                file_name: "may.csv".into(),
                source: "csv".into(),
                stream: "may".into(),
                delimiter: None,
            }
        );
        let given = UploadParams::parse("name=x&source=crm&stream=people&delimiter=tab").unwrap();
        assert_eq!(
            (
                given.source.as_str(),
                given.stream.as_str(),
                given.delimiter
            ),
            ("crm", "people", Some(b'\t'))
        );
        assert_eq!(
            UploadParams::parse("name=x&delimiter=%3B")
                .unwrap()
                .delimiter,
            Some(b';')
        );
    }

    #[test]
    fn a_bad_query_is_refused() {
        let status = |query: &str| UploadParams::parse(query).unwrap_err().status();

        assert_eq!(status(""), StatusCode::BAD_REQUEST);
        assert_eq!(status("name=x&delimiter=pipe"), StatusCode::BAD_REQUEST);
        assert_eq!(status("name=%20"), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            status(&format!("name={}", "n".repeat(129))),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            status("name=x&source=CRM"),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            status("name=x&stream=a%20b"),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[test]
    fn a_header_with_a_repeated_or_blank_name_is_a_422() {
        let duplicate = header_problem(HeaderError::Duplicate);
        let blank = header_problem(HeaderError::Blank);

        assert_eq!(duplicate.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(duplicate.detail(), "body: header names must be unique");
        assert_eq!(blank.detail(), "body: header names must not be blank");
    }
}
