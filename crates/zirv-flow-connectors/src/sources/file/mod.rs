//! The `file` source: a CSV, newline-delimited JSON or top-level JSON array file at an HTTPS
//! address (architecture 7.2). One stream, `records`, read in full on every run.
//!
//! The file is fetched in byte-range windows through [`SourceContext::send`], so a file of any
//! size is read in bounded memory and the guarded client, the egress policy and the retries all
//! apply. A window begins at a record boundary and is parsed by a fresh parser; the part after
//! its last complete record is dropped and fetched again at the head of the next window. The
//! checkpoint is the byte offset of the next window's start, so a read resumes anywhere.
//!
//! Errors are static codes. Neither they nor anything else this module logs holds a record
//! value, the address or a credential.

mod csv;
mod error;
mod fetch;
mod json_array;
mod keys;
mod ndjson;

use std::collections::VecDeque;

use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use url::Url;
use zirv_flow_core::csv_file::{MAX_RECORD_BYTES, detect_delimiter};

use self::csv::CsvWindow;
use self::fetch::{Expect, Window, fetch_window};
use self::json_array::JsonArrayWindow;
use self::ndjson::NdjsonWindow;
use super::context::SourceContext;
use super::credential::Credential;
use super::error::SourceError;
use super::source::Source;
use super::spec::{Capabilities, FieldKind, FieldSpec, SourceSpec};
use super::types::{
    Batch, Checkpoint, FieldDescriptor, FieldType, Record, StreamConfig, StreamDescriptor, SyncMode,
};
use super::validate::validate_config;
use crate::runtime::Auth;

/// The size of one byte-range request: below the client's body cap, and inside its timeout at
/// about 140 KiB/s.
pub(crate) const FILE_WINDOW_BYTES: u64 = 4 * 1024 * 1024;

/// The size of the pieces a window is parsed in; each piece's completed records form a batch.
pub(crate) const FILE_SLICE_BYTES: usize = 64 * 1024;

/// The records discovery looks at to type the fields of a JSON file.
const DISCOVER_RECORDS: usize = 100;

/// The one stream the source reports.
const STREAM: &str = "records";

pub(crate) struct File;

impl Source for File {
    const SPEC: SourceSpec = SourceSpec {
        kind: "file",
        label: "File over HTTPS",
        description: "A CSV, newline-delimited JSON or JSON array file at an https address.",
        config_fields: &[
            FieldSpec {
                key: "url",
                label: "File address",
                help: "The https address of the file. A signed link goes in the Query credential instead.",
                kind: FieldKind::HttpsUrl,
                required: true,
                secret: false,
            },
            FieldSpec {
                key: "format",
                label: "Format",
                help: "csv, json for a top-level array of objects, or ndjson for one object per line.",
                kind: FieldKind::Select(&["csv", "json", "ndjson"]),
                required: true,
                secret: false,
            },
        ],
        credential_fields: &[
            FieldSpec {
                key: "token",
                label: "Bearer token",
                help: "Sent as a Bearer token. Used instead of a user name and password.",
                kind: FieldKind::Text,
                required: false,
                secret: true,
            },
            FieldSpec {
                key: "username",
                label: "User name",
                help: "For basic authentication; needs the password too.",
                kind: FieldKind::Text,
                required: false,
                secret: true,
            },
            FieldSpec {
                key: "password",
                label: "Password",
                help: "For basic authentication; needs the user name too.",
                kind: FieldKind::Text,
                required: false,
                secret: true,
            },
            FieldSpec {
                key: "query",
                label: "Query",
                help: "A query string appended to the address, for a signed link: the signature stays out of the plain settings.",
                kind: FieldKind::Text,
                required: false,
                secret: true,
            },
        ],
        rate_limit: None,
        capabilities: Capabilities {
            incremental: false,
            full_refresh: true,
            emits_deletes: false,
        },
    };

    async fn check(
        &self,
        ctx: &SourceContext,
        config: &Value,
        credential: &Credential,
    ) -> Result<(), SourceError> {
        let target = Target::new(config, credential)?;
        let auth = auth(credential)?;
        fetch_window(ctx, &auth, &target.url, 0, 1024, &Expect::default()).await?;
        Ok(())
    }

    async fn discover(
        &self,
        ctx: &SourceContext,
        config: &Value,
        credential: &Credential,
    ) -> Result<Vec<StreamDescriptor>, SourceError> {
        discover(ctx, config, credential, FILE_WINDOW_BYTES, FILE_SLICE_BYTES).await
    }

    fn read<'a>(
        &'a self,
        ctx: &'a SourceContext,
        config: &'a Value,
        credential: &'a Credential,
        stream: &'a StreamConfig,
        checkpoint: Option<Checkpoint>,
    ) -> BoxStream<'a, Result<Batch, SourceError>> {
        read(
            ctx,
            config,
            credential,
            stream,
            checkpoint,
            FILE_WINDOW_BYTES,
            FILE_SLICE_BYTES,
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Format {
    Csv,
    Json,
    Ndjson,
}

/// The address and format of a configured file.
struct Target {
    /// The address, with the sealed query appended.
    url: Url,
    format: Format,
}

impl Target {
    fn new(config: &Value, credential: &Credential) -> Result<Self, SourceError> {
        if let Err(errors) = validate_config(&File::SPEC, config)
            && let Some(first) = errors.into_iter().next()
        {
            return Err(SourceError::Config {
                field: first.field,
                reason: first.reason,
            });
        }
        let config_error = |field: &str| SourceError::Config {
            field: field.to_owned(),
            reason: "is not usable".to_owned(),
        };
        let mut url = config["url"]
            .as_str()
            .and_then(|text| Url::parse(text).ok())
            .ok_or_else(|| config_error("url"))?;
        let format = match config["format"].as_str() {
            Some("csv") => Format::Csv,
            Some("json") => Format::Json,
            Some("ndjson") => Format::Ndjson,
            _ => return Err(config_error("format")),
        };
        if let Some(query) = credential.get("query") {
            let query = query.expose().trim_start_matches(['?', '&']);
            let joined = match url.query() {
                Some(existing) if !existing.is_empty() => format!("{existing}&{query}"),
                _ => query.to_owned(),
            };
            url.set_query(Some(&joined));
        }
        Ok(Self { url, format })
    }
}

/// The authentication the credential asks for: a token, else a user name with a password,
/// else none.
fn auth(credential: &Credential) -> Result<Auth<'_>, SourceError> {
    if let Some(token) = credential.get("token") {
        return Ok(Auth::Bearer { token });
    }
    match (credential.get("username"), credential.get("password")) {
        (Some(username), Some(password)) => Ok(Auth::Basic {
            username: username.expose(),
            password,
        }),
        (None, None) => Ok(Auth::None),
        (Some(_), None) => Err(SourceError::Config {
            field: "password".to_owned(),
            reason: "is required with a user name".to_owned(),
        }),
        (None, Some(_)) => Err(SourceError::Config {
            field: "username".to_owned(),
            reason: "is required with a password".to_owned(),
        }),
    }
}

/// One record out of a window, and where in the window it ended.
pub(super) struct Parsed {
    pub(super) record: Map<String, Value>,
    /// The bytes of the window done with once this record is: where a read resumes after it.
    pub(super) end: u64,
    /// The record's number in the file, counting from 1.
    pub(super) ordinal: u64,
}

/// A window parser of one of the formats.
enum Parser {
    Csv(Box<CsvWindow>),
    Ndjson(NdjsonWindow),
    Json(JsonArrayWindow),
}

impl Parser {
    /// A parser for a window at the start of the file, which `bytes` begin; `records` came before it.
    fn at_start(format: Format, bytes: &[u8], records: u64) -> Self {
        match format {
            Format::Csv => {
                let sample = &bytes[..bytes.len().min(4096)];
                Self::Csv(Box::new(CsvWindow::at_start(detect_delimiter(sample))))
            }
            Format::Ndjson => Self::Ndjson(NdjsonWindow::new(true, records)),
            Format::Json => Self::Json(JsonArrayWindow::new(true, records)),
        }
    }

    fn push(&mut self, slice: &[u8], out: &mut Vec<Parsed>) -> Result<(), SourceError> {
        match self {
            Self::Csv(window) => window.push(slice, out),
            Self::Ndjson(window) => window.push(slice, out),
            Self::Json(window) => window.push(slice, out),
        }
    }

    fn finish(&mut self, out: &mut Vec<Parsed>) -> Result<(), SourceError> {
        match self {
            Self::Csv(window) => window.finish(out),
            Self::Ndjson(window) => window.finish(out),
            Self::Json(window) => window.finish(out),
        }
    }

    /// Bytes of the window that are done with.
    fn consumed(&self) -> u64 {
        match self {
            Self::Csv(window) => window.consumed(),
            Self::Ndjson(window) => window.consumed(),
            Self::Json(window) => window.consumed(),
        }
    }

    /// The records handed over so far in the file.
    fn ordinal(&self) -> u64 {
        match self {
            Self::Csv(window) => window.ordinal(),
            Self::Ndjson(window) => window.ordinal(),
            Self::Json(window) => window.ordinal(),
        }
    }

    fn header(&self) -> Option<&[String]> {
        match self {
            Self::Csv(window) => window.header(),
            _ => None,
        }
    }
}

/// The checkpoint this source writes (architecture 7.2: opaque to the engine).
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct FileCheckpoint {
    /// The byte offset of the next record.
    offset: u64,
    /// The data records before it.
    row: u64,
    validator: Option<String>,
    size: Option<u64>,
    /// The column names, for CSV.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    header: Option<Vec<String>>,
    /// The delimiter, for CSV.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delimiter: Option<String>,
}

/// What a read carries from one window to the next.
struct Read<'a> {
    ctx: &'a SourceContext,
    credential: &'a Credential,
    target: Target,
    primary_key: Vec<String>,
    window: u64,
    slice: usize,
    /// The next window starts here.
    offset: u64,
    /// The data records before `offset`.
    row: u64,
    /// What the first window saw; later windows must see the same.
    expect: Expect,
    seen: bool,
    header: Option<Vec<String>>,
    delimiter: u8,
    pending: VecDeque<Result<Batch, SourceError>>,
    done: bool,
}

#[allow(clippy::too_many_arguments)]
fn read<'a>(
    ctx: &'a SourceContext,
    config: &'a Value,
    credential: &'a Credential,
    stream: &'a StreamConfig,
    checkpoint: Option<Checkpoint>,
    window: u64,
    slice: usize,
) -> BoxStream<'a, Result<Batch, SourceError>> {
    let start = Target::new(config, credential).and_then(|target| {
        let from = match checkpoint {
            Some(Checkpoint(value)) => {
                serde_json::from_value(value).map_err(|_| error::checkpoint_invalid())?
            }
            None => FileCheckpoint::default(),
        };
        Ok((target, from))
    });
    let state = match start {
        Ok((target, from)) => Read {
            ctx,
            credential,
            target,
            primary_key: stream.primary_key.clone(),
            window,
            slice,
            offset: from.offset,
            row: from.row,
            seen: false,
            // A read that resumes already knows what the file looked like.
            done: from.size == Some(from.offset) && from.offset > 0,
            expect: Expect {
                validator: from.validator,
                size: from.size,
            },
            header: from.header,
            delimiter: from
                .delimiter
                .and_then(|text| text.bytes().next())
                .unwrap_or(b','),
            pending: VecDeque::new(),
        },
        Err(error) => {
            return stream::once(async move { Err(error) }).boxed();
        }
    };
    stream::unfold(state, |mut state| async move {
        loop {
            if let Some(item) = state.pending.pop_front() {
                return Some((item, state));
            }
            if state.done {
                return None;
            }
            if let Err(error) = state.next_window().await {
                state.done = true;
                state.pending.push_back(Err(error));
            }
        }
    })
    .boxed()
}

impl Read<'_> {
    /// Fetches the next window and queues the batches of its records.
    async fn next_window(&mut self) -> Result<(), SourceError> {
        let auth = auth(self.credential)?;
        let mut size = self.window;
        loop {
            let window = fetch_window(
                self.ctx,
                &auth,
                &self.target.url,
                self.offset,
                size,
                &self.expect,
            )
            .await?;
            if !self.seen {
                // What the checkpoint did not carry is what this first window says.
                self.seen = true;
                self.expect = Expect {
                    validator: self.expect.validator.take().or(window.validator.clone()),
                    size: self.expect.size.or(window.total),
                };
            }
            let mut parser = self.parser(&window)?;
            let batches = self.parse(&window, &mut parser)?;
            let consumed = if window.ended {
                window.bytes.len() as u64
            } else {
                parser.consumed()
            };
            if consumed == 0 && !window.ended {
                // No complete record in the window: it is smaller than a record. A window of
                // production size never is, so past twice the record cap the record is too large.
                if size >= 2 * MAX_RECORD_BYTES as u64 {
                    return Err(error::record_too_large());
                }
                size = size.saturating_mul(2);
                continue;
            }
            self.pending.extend(batches.into_iter().map(Ok));
            self.offset += consumed;
            self.row = parser.ordinal();
            if let Parser::Csv(csv) = &parser {
                self.header = csv.header().map(<[String]>::to_vec);
                self.delimiter = csv.delimiter();
            }
            self.done = window.ended;
            return Ok(());
        }
    }

    fn parser(&self, window: &Window) -> Result<Parser, SourceError> {
        if self.offset == 0 {
            return Ok(Parser::at_start(
                self.target.format,
                &window.bytes,
                self.row,
            ));
        }
        Ok(match self.target.format {
            Format::Csv => {
                let header = self.header.clone().ok_or_else(error::checkpoint_invalid)?;
                Parser::Csv(Box::new(CsvWindow::resumed(
                    self.delimiter,
                    header,
                    self.row,
                )))
            }
            Format::Ndjson => Parser::Ndjson(NdjsonWindow::new(false, self.row)),
            Format::Json => Parser::Json(JsonArrayWindow::new(false, self.row)),
        })
    }

    /// Parses the window in slices: the records a slice completes form one batch, whose
    /// checkpoint is the offset after the last of them.
    fn parse(&self, window: &Window, parser: &mut Parser) -> Result<Vec<Batch>, SourceError> {
        let mut batches = Vec::new();
        let mut parsed = Vec::new();
        for slice in window.bytes.chunks(self.slice) {
            parser.push(slice, &mut parsed)?;
            self.batch(&mut parsed, parser, None, &mut batches);
        }
        if window.ended {
            parser.finish(&mut parsed)?;
            let end = window.bytes.len() as u64;
            self.batch(&mut parsed, parser, Some(end), &mut batches);
        }
        Ok(batches)
    }

    /// Turns the parsed records into a batch. `end` is where in the window the batch ends when
    /// that is not where its last record does (the end of the file).
    fn batch(
        &self,
        parsed: &mut Vec<Parsed>,
        parser: &Parser,
        end: Option<u64>,
        batches: &mut Vec<Batch>,
    ) {
        let Some(last) = parsed.last() else {
            return;
        };
        let csv = match parser {
            Parser::Csv(csv) => Some(csv),
            _ => None,
        };
        let checkpoint = FileCheckpoint {
            offset: self.offset + end.unwrap_or(last.end),
            row: last.ordinal,
            validator: self.expect.validator.clone(),
            size: self.expect.size,
            header: csv.and_then(|csv| csv.header().map(<[String]>::to_vec)),
            delimiter: csv.map(|csv| char::from(csv.delimiter()).to_string()),
        };
        let records = parsed
            .drain(..)
            .map(|item| Record {
                key: keys::record_key(&self.primary_key, &item.record, item.ordinal),
                data: Value::Object(item.record),
                deleted: false,
            })
            .collect();
        batches.push(Batch {
            records,
            checkpoint: Checkpoint(serde_json::to_value(checkpoint).unwrap_or(Value::Null)),
        });
    }
}

/// Reads the first window and describes the one stream.
async fn discover(
    ctx: &SourceContext,
    config: &Value,
    credential: &Credential,
    window: u64,
    slice: usize,
) -> Result<Vec<StreamDescriptor>, SourceError> {
    let target = Target::new(config, credential)?;
    let auth = auth(credential)?;
    let first = fetch_window(ctx, &auth, &target.url, 0, window, &Expect::default()).await?;
    let mut parser = Parser::at_start(target.format, &first.bytes, 0);
    let mut records = Vec::new();
    let enough = |parser: &Parser, records: &[Parsed]| match target.format {
        Format::Csv => parser.header().is_some(),
        _ => records.len() >= DISCOVER_RECORDS,
    };
    for piece in first.bytes.chunks(slice) {
        parser.push(piece, &mut records)?;
        if enough(&parser, &records) {
            break;
        }
    }
    if first.ended && !enough(&parser, &records) {
        parser.finish(&mut records)?;
    }
    let fields = match parser.header() {
        Some(header) => header
            .iter()
            .map(|name| FieldDescriptor {
                name: name.clone(),
                field_type: FieldType::String,
            })
            .collect(),
        None if target.format == Format::Csv && !first.bytes.is_empty() => {
            // A header longer than the window, or none at all.
            return Err(error::csv_invalid());
        }
        None => infer_fields(
            records
                .iter()
                .take(DISCOVER_RECORDS)
                .map(|item| &item.record),
        ),
    };
    Ok(vec![StreamDescriptor {
        name: STREAM.to_owned(),
        fields,
        sync_modes: vec![SyncMode::FullRefresh],
        default_cursor_field: None,
        default_primary_key: Vec::new(),
    }])
}

/// The union of the top-level keys of `records`, typed from the first value that is not null.
fn infer_fields<'a>(records: impl Iterator<Item = &'a Map<String, Value>>) -> Vec<FieldDescriptor> {
    let mut fields: Vec<FieldDescriptor> = Vec::new();
    for record in records {
        for (name, value) in record {
            let seen = match value {
                Value::Null => FieldType::Unknown,
                Value::String(_) => FieldType::String,
                Value::Number(_) => FieldType::Number,
                Value::Bool(_) => FieldType::Boolean,
                Value::Array(_) | Value::Object(_) => FieldType::Json,
            };
            match fields.iter_mut().find(|field| &field.name == name) {
                Some(field) if field.field_type == FieldType::Unknown => field.field_type = seen,
                Some(_) => {}
                None => fields.push(FieldDescriptor {
                    name: name.clone(),
                    field_type: seen,
                }),
            }
        }
    }
    fields
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::extract::State;
    use axum::http::{HeaderMap, Response, StatusCode};
    use axum::response::{IntoResponse, Redirect};
    use axum::routing::get;
    use serde_json::json;
    use zirv_flow_core::SecretString;
    use zirv_flow_core::egress::EgressPolicy;

    use super::*;
    use crate::http::{HttpError, HttpLimits};
    use crate::runtime::RetryPolicy;

    /// What the test server saw of one request.
    #[derive(Clone, Debug)]
    struct Logged {
        path_and_query: String,
        range: Option<String>,
        if_range: Option<String>,
        authorization: Option<String>,
    }

    /// A file served with byte ranges, and the ways a server can misbehave.
    struct Fixture {
        body: Vec<u8>,
        etag: Option<&'static str>,
        ignore_range: bool,
        /// Requests from this one on (counting from 0) see the changed file.
        change_after: usize,
        changed_body: Option<Vec<u8>>,
        changed_etag: Option<&'static str>,
        /// Requests from this one on answer 412.
        precondition_failed_after: usize,
        required_authorization: Option<String>,
        /// The first requests answer 429 with `Retry-After: 0`.
        throttled: usize,
        hits: AtomicUsize,
        log: Mutex<Vec<Logged>>,
    }

    impl Fixture {
        fn new(body: impl Into<Vec<u8>>) -> Self {
            Self {
                body: body.into(),
                etag: Some("\"v1\""),
                ignore_range: false,
                change_after: usize::MAX,
                changed_body: None,
                changed_etag: None,
                precondition_failed_after: usize::MAX,
                required_authorization: None,
                throttled: 0,
                hits: AtomicUsize::new(0),
                log: Mutex::new(Vec::new()),
            }
        }

        fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }

        fn log(&self) -> Vec<Logged> {
            self.log.lock().unwrap().clone()
        }
    }

    fn header(headers: &HeaderMap, name: &str) -> Option<String> {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }

    async fn serve_file(
        State(fixture): State<Arc<Fixture>>,
        uri: axum::http::Uri,
        headers: HeaderMap,
    ) -> Response<Body> {
        let n = fixture.hits.fetch_add(1, Ordering::SeqCst);
        fixture.log.lock().unwrap().push(Logged {
            path_and_query: uri
                .path_and_query()
                .map(ToString::to_string)
                .unwrap_or_default(),
            range: header(&headers, "range"),
            if_range: header(&headers, "if-range"),
            authorization: header(&headers, "authorization"),
        });
        if let Some(expected) = &fixture.required_authorization
            && header(&headers, "authorization").as_ref() != Some(expected)
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        if n < fixture.throttled {
            return (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "0")]).into_response();
        }
        if n >= fixture.precondition_failed_after {
            return StatusCode::PRECONDITION_FAILED.into_response();
        }
        let changed = n >= fixture.change_after;
        let body = match (&fixture.changed_body, changed) {
            (Some(body), true) => body.clone(),
            _ => fixture.body.clone(),
        };
        let etag = if changed && fixture.changed_etag.is_some() {
            fixture.changed_etag
        } else {
            fixture.etag
        };
        let with_etag = |mut builder: axum::http::response::Builder| {
            if let Some(etag) = etag {
                builder = builder.header("etag", etag);
            }
            builder
        };
        let whole = |body: Vec<u8>| {
            with_etag(Response::builder().status(200))
                .body(Body::from(body))
                .unwrap()
        };
        let Some(range) = header(&headers, "range") else {
            return whole(body);
        };
        if fixture.ignore_range {
            return whole(body);
        }
        if let (Some(asked), Some(current)) = (header(&headers, "if-range"), etag)
            && asked != current
        {
            return whole(body);
        }
        let spec = range.strip_prefix("bytes=").unwrap_or_default();
        let (first, last) = spec.split_once('-').unwrap_or_default();
        let first: usize = first.parse().unwrap_or(0);
        let last: usize = last.parse().unwrap_or(usize::MAX);
        if first >= body.len() {
            return Response::builder()
                .status(416)
                .header("content-range", format!("bytes */{}", body.len()))
                .body(Body::empty())
                .unwrap();
        }
        let last = last.min(body.len() - 1);
        with_etag(Response::builder().status(206).header(
            "content-range",
            format!("bytes {first}-{last}/{}", body.len()),
        ))
        .body(Body::from(body[first..=last].to_vec()))
        .unwrap()
    }

    /// Serves `fixture` on a loopback port; its address is `http://127.0.0.1:<port>/f`.
    async fn serve(fixture: Fixture) -> (String, Arc<Fixture>) {
        let fixture = Arc::new(fixture);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let router =
            Router::new()
                .route("/f", get(serve_file))
                .route(
                    "/redirect",
                    get(move || async move {
                        Redirect::temporary(&format!("http://localhost:{port}/f"))
                    }),
                )
                .with_state(Arc::clone(&fixture));
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (format!("http://127.0.0.1:{port}"), fixture)
    }

    fn context() -> SourceContext {
        let policy = Arc::new(EgressPolicy::parse(&["127.0.0.1/32".to_owned()]).unwrap());
        SourceContext::new(
            policy,
            HttpLimits::default(),
            "file",
            "src-1",
            Arc::default(),
        )
        .unwrap()
        .with_retry_policy(RetryPolicy {
            max_attempts: 5,
            base: Duration::from_millis(5),
            cap: Duration::from_millis(20),
            max_wait: Duration::from_secs(300),
        })
    }

    fn config(base: &str, format: &str) -> Value {
        json!({"url": format!("{base}/f"), "format": format})
    }

    fn credential(fields: &[(&str, &str)]) -> Credential {
        let mut credential = Credential::new();
        for (key, value) in fields {
            credential.insert(*key, SecretString::new((*value).into()));
        }
        credential
    }

    fn stream(primary_key: &[&str]) -> StreamConfig {
        StreamConfig {
            name: STREAM.into(),
            sync_mode: SyncMode::FullRefresh,
            cursor_field: None,
            primary_key: primary_key.iter().map(|key| (*key).to_owned()).collect(),
        }
    }

    async fn read_all(
        ctx: &SourceContext,
        config: &Value,
        credential: &Credential,
        stream: &StreamConfig,
        checkpoint: Option<Checkpoint>,
        window: u64,
        slice: usize,
    ) -> Vec<Result<Batch, SourceError>> {
        read(ctx, config, credential, stream, checkpoint, window, slice)
            .collect()
            .await
    }

    fn batches(results: Vec<Result<Batch, SourceError>>) -> Vec<Batch> {
        results.into_iter().map(|result| result.unwrap()).collect()
    }

    fn data(batches: &[Batch]) -> Vec<Value> {
        batches
            .iter()
            .flat_map(|batch| batch.records.iter().map(|record| record.data.clone()))
            .collect()
    }

    fn keys(batches: &[Batch]) -> Vec<String> {
        batches
            .iter()
            .flat_map(|batch| batch.records.iter().map(|record| record.key.clone()))
            .collect()
    }

    /// CSV with a byte order mark, CRLF, quoted delimiters, quotes and line breaks.
    fn csv_rows() -> (Vec<u8>, Vec<Value>) {
        let rows: Vec<[String; 3]> = (0..25)
            .map(|n| {
                let note = match n % 5 {
                    0 => format!("plain {n}"),
                    1 => format!("with, comma {n}"),
                    2 => format!("with \"quote\" {n}"),
                    3 => format!("line\r\nbreak {n}"),
                    _ => String::new(),
                };
                [format!("id-{n}"), format!("name {n}"), note]
            })
            .collect();
        let mut text = String::from("\u{FEFF}id,name,note\r\n");
        let mut expected = Vec::new();
        for row in &rows {
            let cells: Vec<String> = row
                .iter()
                .map(|cell| {
                    let mut out = String::new();
                    zirv_flow_core::csv_file::write_field(&mut out, cell, ',');
                    out
                })
                .collect();
            text.push_str(&cells.join(","));
            text.push_str("\r\n");
            expected.push(json!({"id": row[0], "name": row[1], "note": row[2]}));
        }
        (text.into_bytes(), expected)
    }

    #[tokio::test]
    async fn a_csv_is_read_whole_across_windows_of_any_size_including_quotes_and_crlf_on_a_cut() {
        let (body, expected) = csv_rows();
        let (base, fixture) = serve(Fixture::new(body)).await;
        let config = config(&base, "csv");

        for window in [1_u64, 2, 3, 5, 8, 13, 21, 64, 100, 1000, 100_000] {
            for slice in [1_usize, 7, 64 * 1024] {
                let results = read_all(
                    &context(),
                    &config,
                    &Credential::new(),
                    &stream(&["id"]),
                    None,
                    window,
                    slice,
                )
                .await;

                let batches = batches(results);
                assert_eq!(data(&batches), expected, "window {window}, slice {slice}");
                let expected_keys: Vec<_> = (0..25).map(|n| format!("id-{n}")).collect();
                assert_eq!(keys(&batches), expected_keys, "window {window}");
                let offsets: Vec<u64> = batches
                    .iter()
                    .map(|batch| batch.checkpoint.0["offset"].as_u64().unwrap())
                    .collect();
                assert!(
                    offsets.windows(2).all(|pair| pair[0] < pair[1]),
                    "{offsets:?}"
                );
            }
        }
        assert!(fixture.hits() > 0);
    }

    #[tokio::test]
    async fn a_semicolon_and_a_tab_delimited_file_are_detected_and_resumed_with_their_delimiter() {
        for (delimiter, name) in [(";", "semicolon"), ("\t", "tab")] {
            let body = format!("a{delimiter}b\n1{delimiter}x\n2{delimiter}y\n3{delimiter}z\n");
            let (base, _) = serve(Fixture::new(body)).await;
            let config = config(&base, "csv");

            let full = batches(
                read_all(
                    &context(),
                    &config,
                    &Credential::new(),
                    &stream(&[]),
                    None,
                    8,
                    4,
                )
                .await,
            );
            let resumed = batches(
                read_all(
                    &context(),
                    &config,
                    &Credential::new(),
                    &stream(&[]),
                    Some(full[0].checkpoint.clone()),
                    8,
                    4,
                )
                .await,
            );

            assert_eq!(data(&full).len(), 3, "{name}");
            assert_eq!(data(&full)[2], json!({"a": "3", "b": "z"}), "{name}");
            assert_eq!(
                full[0].checkpoint.0["delimiter"],
                json!(delimiter),
                "{name}"
            );
            assert_eq!(
                data(&resumed),
                data(&full)[full[0].records.len()..],
                "{name}"
            );
        }
    }

    fn json_records() -> Vec<Value> {
        vec![
            json!({"id": 1, "name": "plain", "tags": ["a", "b"], "address": {"city": "Aarhus"}}),
            json!({"id": 2, "name": "]}, \" \\ [ {", "tags": [], "address": null}),
            json!({"id": 3, "name": "é ✓ \u{1F600}", "tags": [{"x": "]"}], "address": {"city": "x"}}),
            json!({"id": 4, "name": "", "extra": true}),
        ]
    }

    fn array_body() -> Vec<u8> {
        let items: Vec<String> = json_records().iter().map(Value::to_string).collect();
        format!("\u{FEFF}[\n  {}\n]\n", items.join(",\n  ")).into_bytes()
    }

    fn ndjson_body() -> Vec<u8> {
        let lines: Vec<String> = json_records().iter().map(Value::to_string).collect();
        format!("{}\r\n\r\n{}", lines[..3].join("\r\n"), lines[3]).into_bytes()
    }

    #[tokio::test]
    async fn a_json_array_and_ndjson_give_the_same_records_wherever_the_windows_cut() {
        for (format, body) in [("json", array_body()), ("ndjson", ndjson_body())] {
            let (base, _) = serve(Fixture::new(body)).await;
            let config = config(&base, format);
            for window in [1_u64, 2, 3, 7, 16, 33, 64, 1000, 1 << 20] {
                let results = read_all(
                    &context(),
                    &config,
                    &Credential::new(),
                    &stream(&["id"]),
                    None,
                    window,
                    5,
                )
                .await;

                let batches = batches(results);
                assert_eq!(data(&batches), json_records(), "{format}, window {window}");
                assert_eq!(keys(&batches), ["1", "2", "3", "4"], "{format}");
            }
        }
    }

    #[tokio::test]
    async fn a_resumed_read_equals_the_tail_of_an_uninterrupted_one_for_every_window_size() {
        let (csv_body, _) = csv_rows();
        let formats = [
            ("csv", csv_body, "id"),
            ("json", array_body(), "id"),
            ("ndjson", ndjson_body(), "id"),
        ];
        for (format, body, key) in formats {
            let (base, _) = serve(Fixture::new(body)).await;
            let config = config(&base, format);
            for window in 1_u64..=64 {
                let all = batches(
                    read_all(
                        &context(),
                        &config,
                        &Credential::new(),
                        &stream(&[key]),
                        None,
                        window,
                        3,
                    )
                    .await,
                );
                let mut seen = 0;
                for (index, batch) in all.iter().enumerate() {
                    seen += batch.records.len();
                    // Every third checkpoint keeps the loop cheap and still crosses every cut.
                    if index % 3 != 0 {
                        continue;
                    }
                    let resumed = batches(
                        read_all(
                            &context(),
                            &config,
                            &Credential::new(),
                            &stream(&[key]),
                            Some(batch.checkpoint.clone()),
                            window,
                            3,
                        )
                        .await,
                    );
                    assert_eq!(
                        data(&resumed),
                        data(&all)[seen..],
                        "{format}, window {window}, after batch {index}"
                    );
                    assert_eq!(
                        keys(&resumed),
                        keys(&all)[seen..],
                        "{format}, window {window}"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn a_positional_key_numbers_the_records_across_windows_and_resumes() {
        let (base, _) = serve(Fixture::new("a\n1\n2\n3\n4\n5\n")).await;
        let config = config(&base, "csv");

        let all = batches(
            read_all(
                &context(),
                &config,
                &Credential::new(),
                &stream(&[]),
                None,
                6,
                2,
            )
            .await,
        );
        let resumed = batches(
            read_all(
                &context(),
                &config,
                &Credential::new(),
                &stream(&[]),
                Some(all[1].checkpoint.clone()),
                6,
                2,
            )
            .await,
        );

        assert_eq!(keys(&all), ["row:1", "row:2", "row:3", "row:4", "row:5"]);
        assert_eq!(
            keys(&resumed),
            keys(&all)[all[0].records.len() + all[1].records.len()..]
        );
    }

    #[tokio::test]
    async fn several_primary_key_fields_make_one_unambiguous_key() {
        let (base, _) = serve(Fixture::new("a,b\nx,\"y,z\"\n\"x,y\",z\n")).await;

        let all = batches(
            read_all(
                &context(),
                &config(&base, "csv"),
                &Credential::new(),
                &stream(&["a", "b"]),
                None,
                1000,
                1000,
            )
            .await,
        );

        let keys = keys(&all);
        assert_eq!(keys[0], r#"["x","y,z"]"#);
        assert_eq!(keys[1], r#"["x,y","z"]"#);
    }

    #[tokio::test]
    async fn a_slice_that_completes_records_is_one_batch_and_one_that_completes_none_is_none() {
        let (base, _) = serve(Fixture::new("a\n1\n2\n3\n4\n")).await;

        let all = batches(
            read_all(
                &context(),
                &config(&base, "csv"),
                &Credential::new(),
                &stream(&[]),
                None,
                1000,
                4,
            )
            .await,
        );

        // "a\n1\n" completes the header and a record; then one record per further slice.
        let sizes: Vec<_> = all.iter().map(|batch| batch.records.len()).collect();
        assert_eq!(sizes, [1, 2, 1]);
        assert_eq!(all.last().unwrap().checkpoint.0["offset"], json!(10));
        assert_eq!(all.last().unwrap().checkpoint.0["row"], json!(4));
    }

    async fn collect_error(results: Vec<Result<Batch, SourceError>>) -> (Vec<Batch>, SourceError) {
        let mut ok = Vec::new();
        for result in results {
            match result {
                Ok(batch) => ok.push(batch),
                Err(error) => return (ok, error),
            }
        }
        panic!("the read did not fail");
    }

    #[tokio::test]
    async fn a_file_that_changes_between_windows_fails_the_read_and_keeps_what_was_committed() {
        let body: String = (0..30).map(|n| format!("{n}\n")).collect();
        let body = format!("a\n{body}");
        let cases: Vec<(&str, Fixture)> = vec![
            ("etag flips", {
                let mut fixture = Fixture::new(body.clone());
                fixture.change_after = 2;
                fixture.changed_etag = Some("\"v2\"");
                fixture
            }),
            ("size flips", {
                let mut fixture = Fixture::new(body.clone());
                fixture.etag = None;
                fixture.change_after = 2;
                fixture.changed_body = Some(format!("{body}99\n").into_bytes());
                fixture
            }),
            ("412", {
                let mut fixture = Fixture::new(body.clone());
                fixture.precondition_failed_after = 2;
                fixture
            }),
        ];
        for (name, fixture) in cases {
            let (base, _) = serve(fixture).await;

            let (committed, error) = collect_error(
                read_all(
                    &context(),
                    &config(&base, "csv"),
                    &Credential::new(),
                    &stream(&[]),
                    None,
                    20,
                    10,
                )
                .await,
            )
            .await;

            assert_eq!(error, SourceError::Other("file_changed".into()), "{name}");
            assert!(!committed.is_empty(), "{name}: the first windows stay");
            assert!(data(&committed).len() < 30, "{name}");
        }
    }

    #[tokio::test]
    async fn a_resume_checks_the_validator_and_size_of_the_checkpoint() {
        let body: String = (0..30).map(|n| format!("{n}\n")).collect();
        let body = format!("a\n{body}");
        let (base, _) = serve(Fixture::new(body.clone())).await;
        let config = config(&base, "csv");
        let first = batches(
            read_all(
                &context(),
                &config,
                &Credential::new(),
                &stream(&[]),
                None,
                20,
                10,
            )
            .await,
        );
        let mut checkpoint = first[0].checkpoint.clone();
        assert_eq!(checkpoint.0["validator"], json!("\"v1\""));
        assert_eq!(checkpoint.0["size"], json!(body.len()));

        // The file under the checkpoint is another one now.
        checkpoint.0["validator"] = json!("\"stale\"");
        let (_, error) = collect_error(
            read_all(
                &context(),
                &config,
                &Credential::new(),
                &stream(&[]),
                Some(checkpoint.clone()),
                20,
                10,
            )
            .await,
        )
        .await;
        assert_eq!(error, SourceError::Other("file_changed".into()));

        checkpoint.0["validator"] = json!("\"v1\"");
        checkpoint.0["size"] = json!(body.len() + 5);
        let (_, error) = collect_error(
            read_all(
                &context(),
                &config,
                &Credential::new(),
                &stream(&[]),
                Some(checkpoint),
                20,
                10,
            )
            .await,
        )
        .await;
        assert_eq!(error, SourceError::Other("file_changed".into()));
    }

    #[tokio::test]
    async fn a_checkpoint_at_the_end_of_the_file_reads_nothing_and_a_foreign_one_is_refused() {
        let (base, fixture) = serve(Fixture::new("a\n1\n")).await;
        let config = config(&base, "csv");
        let all = batches(
            read_all(
                &context(),
                &config,
                &Credential::new(),
                &stream(&[]),
                None,
                100,
                100,
            )
            .await,
        );
        let before = fixture.hits();

        let again = read_all(
            &context(),
            &config,
            &Credential::new(),
            &stream(&[]),
            Some(all.last().unwrap().checkpoint.clone()),
            100,
            100,
        )
        .await;
        let foreign = read_all(
            &context(),
            &config,
            &Credential::new(),
            &stream(&[]),
            Some(Checkpoint(json!({"cursor": 3}))),
            100,
            100,
        )
        .await;

        assert!(again.is_empty());
        assert_eq!(fixture.hits(), before);
        assert_eq!(foreign.len(), 1);
        assert_eq!(
            foreign[0].as_ref().unwrap_err(),
            &SourceError::Other("checkpoint_invalid".into())
        );
    }

    #[tokio::test]
    async fn a_server_that_ignores_range_serves_a_small_file_whole_and_fails_a_large_one() {
        let mut small = Fixture::new("a\n1\n2\n");
        small.ignore_range = true;
        let (base, _) = serve(small).await;
        let all = batches(
            read_all(
                &context(),
                &config(&base, "csv"),
                &Credential::new(),
                &stream(&[]),
                None,
                FILE_WINDOW_BYTES,
                FILE_SLICE_BYTES,
            )
            .await,
        );
        assert_eq!(data(&all), [json!({"a": "1"}), json!({"a": "2"})]);

        let line = "{\"a\":1}\n";
        let mut large = Fixture::new(line.repeat(11 * 1024 * 1024 / line.len() + 1));
        large.ignore_range = true;
        let (base, _) = serve(large).await;
        let (_, error) = collect_error(
            read_all(
                &context(),
                &config(&base, "ndjson"),
                &Credential::new(),
                &stream(&[]),
                None,
                FILE_WINDOW_BYTES,
                FILE_SLICE_BYTES,
            )
            .await,
        )
        .await;
        assert_eq!(error, SourceError::Other("file_range_unsupported".into()));
    }

    #[tokio::test]
    async fn an_empty_file_has_no_records_and_passes_the_check() {
        let (base, _) = serve(Fixture::new("")).await;
        for format in ["csv", "json", "ndjson"] {
            let config = config(&base, format);

            let results = read_all(
                &context(),
                &config,
                &Credential::new(),
                &stream(&[]),
                None,
                100,
                100,
            )
            .await;

            assert!(results.is_empty(), "{format}");
            assert_eq!(
                File.check(&context(), &config, &Credential::new()).await,
                Ok(())
            );
        }
    }

    #[tokio::test]
    async fn a_missing_file_a_refused_credential_and_a_throttled_server_are_told_apart() {
        let (base, _) = serve(Fixture::new("a\n1\n")).await;
        let missing = json!({"url": format!("{base}/nope"), "format": "csv"});
        let (_, error) = collect_error(
            read_all(
                &context(),
                &missing,
                &Credential::new(),
                &stream(&[]),
                None,
                100,
                100,
            )
            .await,
        )
        .await;
        assert_eq!(error, SourceError::Upstream(404));

        let mut guarded = Fixture::new("a\n1\n");
        guarded.required_authorization = Some("Bearer s3cret".into());
        let (base, _) = serve(guarded).await;
        let (_, error) = collect_error(
            read_all(
                &context(),
                &config(&base, "csv"),
                &Credential::new(),
                &stream(&[]),
                None,
                100,
                100,
            )
            .await,
        )
        .await;
        assert_eq!(error, SourceError::Auth);

        let mut throttled = Fixture::new("a\n1\n");
        throttled.throttled = 2;
        let (base, fixture) = serve(throttled).await;
        let all = batches(
            read_all(
                &context(),
                &config(&base, "csv"),
                &Credential::new(),
                &stream(&[]),
                None,
                100,
                100,
            )
            .await,
        );
        assert_eq!(data(&all), [json!({"a": "1"})]);
        assert_eq!(fixture.hits(), 3);
    }

    #[tokio::test]
    async fn a_token_basic_credentials_and_the_sealed_query_are_sent_and_never_shown() {
        let mut guarded = Fixture::new("a\n1\n");
        guarded.required_authorization = Some("Bearer tok-123".into());
        let (base, fixture) = serve(guarded).await;
        let config = config(&base, "csv");
        let with_token = credential(&[("token", "tok-123"), ("query", "?sig=abc&exp=9")]);

        let all = batches(
            read_all(
                &context(),
                &config,
                &with_token,
                &stream(&[]),
                None,
                100,
                100,
            )
            .await,
        );

        assert_eq!(data(&all), [json!({"a": "1"})]);
        let seen = fixture.log();
        assert_eq!(seen[0].authorization.as_deref(), Some("Bearer tok-123"));
        assert_eq!(seen[0].path_and_query, "/f?sig=abc&exp=9");

        let mut basic = Fixture::new("a\n1\n");
        basic.required_authorization = Some("Basic dXNlcjpwdw==".into());
        let (base, _) = serve(basic).await;
        let all = batches(
            read_all(
                &context(),
                &self::config(&base, "csv"),
                &credential(&[("username", "user"), ("password", "pw")]),
                &stream(&[]),
                None,
                100,
                100,
            )
            .await,
        );
        assert_eq!(data(&all).len(), 1);

        // A failing read never repeats the address, the query or the credential.
        let (base, _) = serve(Fixture::new("a\n1\n")).await;
        let missing = json!({"url": format!("{base}/nope?x=1"), "format": "csv"});
        let (_, error) = collect_error(
            read_all(
                &context(),
                &missing,
                &with_token,
                &stream(&[]),
                None,
                100,
                100,
            )
            .await,
        )
        .await;
        let shown = format!("{error} {error:?}");
        for secret in ["sig=abc", "tok-123", "127.0.0.1", "nope"] {
            assert!(!shown.contains(secret), "{shown}");
        }
    }

    #[tokio::test]
    async fn a_user_name_without_a_password_is_a_config_error_naming_the_field() {
        let (base, _) = serve(Fixture::new("a\n1\n")).await;

        let error = File
            .check(
                &context(),
                &config(&base, "csv"),
                &credential(&[("username", "user")]),
            )
            .await
            .unwrap_err();

        assert_eq!(
            error,
            SourceError::Config {
                field: "password".into(),
                reason: "is required with a user name".into()
            }
        );
    }

    #[tokio::test]
    async fn the_default_egress_policy_blocks_loopback_and_no_request_arrives() {
        let (base, fixture) = serve(Fixture::new("a\n1\n")).await;
        let blocked = SourceContext::new(
            Arc::new(EgressPolicy::default()),
            HttpLimits::default(),
            "file",
            "src-1",
            Arc::default(),
        )
        .unwrap();
        let config = config(&base, "csv");

        let read = collect_error(
            read_all(
                &blocked,
                &config,
                &Credential::new(),
                &stream(&[]),
                None,
                100,
                100,
            )
            .await,
        )
        .await
        .1;
        let check = File.check(&blocked, &config, &Credential::new()).await;
        let discover = File.discover(&blocked, &config, &Credential::new()).await;

        for error in [read, check.unwrap_err(), discover.unwrap_err()] {
            assert!(matches!(
                error,
                SourceError::Egress(HttpError::EgressDenied { .. })
            ));
        }
        assert_eq!(fixture.hits(), 0);

        // With the address allowed it works.
        assert_eq!(
            File.check(&context(), &config, &Credential::new()).await,
            Ok(())
        );
    }

    #[tokio::test]
    async fn a_redirect_off_the_origin_is_refused_and_not_followed() {
        let (base, fixture) = serve(Fixture::new("a\n1\n")).await;
        let redirecting = json!({"url": format!("{base}/redirect"), "format": "csv"});

        let (_, error) = collect_error(
            read_all(
                &context(),
                &redirecting,
                &credential(&[("token", "tok")]),
                &stream(&[]),
                None,
                100,
                100,
            )
            .await,
        )
        .await;

        assert!(matches!(error, SourceError::Other(_)), "{error:?}");
        assert!(fixture.log().is_empty(), "the target was never asked");
    }

    #[tokio::test]
    async fn file_problems_are_static_codes_that_never_quote_the_file() {
        let cases: Vec<(&str, &str, Vec<u8>, &str)> = vec![
            (
                "blank header",
                "csv",
                b"a,,c\n1,2,3\n".to_vec(),
                "blank_header",
            ),
            (
                "duplicate header",
                "csv",
                b"a,a\n1,2\n".to_vec(),
                "duplicate_header",
            ),
            (
                "invalid utf-8",
                "csv",
                b"a\nsecret\xFF\xFE\n".to_vec(),
                "csv_invalid",
            ),
            (
                "unclosed quote",
                "csv",
                b"a\n\"secret".to_vec(),
                "csv_invalid",
            ),
            (
                "not json",
                "ndjson",
                b"{\"a\": secret}\n".to_vec(),
                "json_invalid",
            ),
            (
                "not an array",
                "json",
                b"{\"a\": \"secret\"}".to_vec(),
                "json_not_an_array",
            ),
            (
                "trailing data",
                "json",
                b"[{\"a\":1}] secret".to_vec(),
                "json_trailing_data",
            ),
        ];
        for (name, format, body, code) in cases {
            let (base, _) = serve(Fixture::new(body)).await;

            let (_, error) = collect_error(
                read_all(
                    &context(),
                    &config(&base, format),
                    &Credential::new(),
                    &stream(&[]),
                    None,
                    1000,
                    1000,
                )
                .await,
            )
            .await;

            assert_eq!(error, SourceError::Other(code.into()), "{name}");
            assert!(!format!("{error:?}").contains("secret"), "{name}");
        }
    }

    #[tokio::test]
    async fn a_record_over_a_mebibyte_fails_with_record_too_large_in_every_format() {
        let big = "x".repeat((1 << 20) + 10);
        let cases = [
            ("csv", format!("a\n\"{big}\"\n")),
            ("ndjson", format!("{{\"a\":\"{big}\"}}\n")),
            ("json", format!("[{{\"a\":\"{big}\"}}]")),
        ];
        for (format, body) in cases {
            let (base, _) = serve(Fixture::new(body)).await;

            let (_, error) = collect_error(
                read_all(
                    &context(),
                    &config(&base, format),
                    &Credential::new(),
                    &stream(&[]),
                    None,
                    FILE_WINDOW_BYTES,
                    FILE_SLICE_BYTES,
                )
                .await,
            )
            .await;

            assert_eq!(
                error,
                SourceError::Other("record_too_large".into()),
                "{format}"
            );
        }
    }

    #[tokio::test]
    async fn discovery_reports_one_full_refresh_stream_with_typed_fields() {
        let (base, _) = serve(Fixture::new(array_body())).await;
        let streams = File
            .discover(&context(), &config(&base, "json"), &Credential::new())
            .await
            .unwrap();

        assert_eq!(streams.len(), 1);
        let stream = &streams[0];
        assert_eq!(stream.name, "records");
        assert_eq!(stream.sync_modes, [SyncMode::FullRefresh]);
        assert!(stream.default_primary_key.is_empty());
        assert_eq!(stream.default_cursor_field, None);
        let types: Vec<_> = stream
            .fields
            .iter()
            .map(|field| (field.name.as_str(), field.field_type))
            .collect();
        assert_eq!(
            types,
            [
                ("address", FieldType::Json),
                ("id", FieldType::Number),
                ("name", FieldType::String),
                ("tags", FieldType::Json),
                ("extra", FieldType::Boolean),
            ]
        );

        let (csv, _) = csv_rows();
        let (base, _) = serve(Fixture::new(csv)).await;
        let csv_streams = File
            .discover(&context(), &config(&base, "csv"), &Credential::new())
            .await
            .unwrap();
        let names: Vec<_> = csv_streams[0]
            .fields
            .iter()
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(names, ["id", "name", "note"]);
        assert!(
            csv_streams[0]
                .fields
                .iter()
                .all(|f| f.field_type == FieldType::String)
        );
    }

    #[test]
    fn a_null_only_field_is_unknown_until_a_value_types_it() {
        let records = [json!({"a": null, "b": null}), json!({"a": 3, "b": null})];

        let fields = infer_fields(records.iter().map(|value| value.as_object().unwrap()));

        let types: Vec<_> = fields
            .iter()
            .map(|f| (f.name.as_str(), f.field_type))
            .collect();
        assert_eq!(types, [("a", FieldType::Number), ("b", FieldType::Unknown)]);
    }

    #[test]
    fn the_spec_declares_secret_credentials_a_required_url_and_a_full_refresh_only_stream() {
        let spec = &File::SPEC;

        assert_eq!(spec.kind, "file");
        assert!(
            spec.credential_fields
                .iter()
                .all(|f| f.secret && !f.required)
        );
        let keys: Vec<_> = spec.credential_fields.iter().map(|f| f.key).collect();
        assert_eq!(keys, ["token", "username", "password", "query"]);
        assert!(spec.config_fields.iter().all(|f| f.required && !f.secret));
        assert!(!spec.capabilities.incremental);
        assert!(spec.capabilities.full_refresh);
        assert!(!spec.capabilities.emits_deletes);
    }

    #[test]
    fn the_config_is_checked_against_the_spec_and_a_bad_one_names_the_field() {
        let credential = Credential::new();
        let bad = [
            (json!({"format": "csv"}), "url"),
            (json!({"url": "https://example.com/f"}), "format"),
            (
                json!({"url": "https://example.com/f", "format": "xml"}),
                "format",
            ),
            (
                json!({"url": "https://u:p@example.com/f", "format": "csv"}),
                "url",
            ),
            (
                json!({"url": "https://example.com/f#frag", "format": "csv"}),
                "url",
            ),
            (
                json!({"url": "ftp://example.com/f", "format": "csv"}),
                "url",
            ),
            (
                json!({"url": "https://example.com/f", "format": "csv", "x": 1}),
                "x",
            ),
        ];
        for (config, field) in bad {
            match Target::new(&config, &credential) {
                Err(SourceError::Config { field: named, .. }) => assert_eq!(named, field),
                other => panic!("{config}: {:?}", other.err()),
            }
        }
        assert!(
            Target::new(
                &json!({"url": "https://example.com/f", "format": "ndjson"}),
                &credential
            )
            .is_ok()
        );
    }

    #[test]
    fn the_module_never_names_the_http_client_it_sends_through() {
        for (name, source) in [
            ("mod.rs", include_str!("mod.rs")),
            ("fetch.rs", include_str!("fetch.rs")),
            ("csv.rs", include_str!("csv.rs")),
            ("ndjson.rs", include_str!("ndjson.rs")),
            ("json_array.rs", include_str!("json_array.rs")),
            ("keys.rs", include_str!("keys.rs")),
            ("error.rs", include_str!("error.rs")),
        ] {
            let production = source.split("#[cfg(test)]").next().unwrap();
            for forbidden in ["reqwest", "GuardedClient"] {
                assert!(!production.contains(forbidden), "{name} names {forbidden}");
            }
        }
    }

    #[tokio::test]
    async fn every_window_asks_for_its_byte_range_and_later_ones_pin_the_validator_with_if_range() {
        let body: String = (0..40).map(|n| format!("{n}\n")).collect();
        let (base, fixture) = serve(Fixture::new(format!("a\n{body}"))).await;

        let all = batches(
            read_all(
                &context(),
                &config(&base, "csv"),
                &Credential::new(),
                &stream(&[]),
                None,
                30,
                30,
            )
            .await,
        );

        assert_eq!(data(&all).len(), 40);
        let log = fixture.log();
        assert!(log.len() > 2);
        assert_eq!(log[0].range.as_deref(), Some("bytes=0-29"));
        assert_eq!(log[0].if_range, None);
        for (index, request) in log.iter().enumerate().skip(1) {
            assert!(
                request.range.as_deref().unwrap().starts_with("bytes="),
                "request {index}"
            );
            assert_eq!(
                request.if_range.as_deref(),
                Some("\"v1\""),
                "request {index}"
            );
        }
    }

    #[tokio::test]
    async fn a_csv_of_more_than_two_production_windows_syncs_every_row_once() {
        let mut text = String::from("id,note\r\n");
        let mut rows = 0_u64;
        while text.len() < 9 * 1024 * 1024 {
            // Every fifth note is quoted with a delimiter and a line break in it, so the cuts
            // land inside quotes and between CR and LF somewhere in the file.
            let note = if rows.is_multiple_of(5) {
                format!("\"two,\r\nlines {rows} {}\"", "x".repeat(40))
            } else {
                format!("plain {rows} {}", "y".repeat(40))
            };
            text.push_str(&format!("{rows},{note}\r\n"));
            rows += 1;
        }
        let (base, fixture) = serve(Fixture::new(text)).await;

        let all = batches(
            read_all(
                &context(),
                &config(&base, "csv"),
                &Credential::new(),
                &stream(&["id"]),
                None,
                FILE_WINDOW_BYTES,
                FILE_SLICE_BYTES,
            )
            .await,
        );

        let keys = keys(&all);
        assert_eq!(keys.len() as u64, rows);
        let expected: Vec<String> = (0..rows).map(|n| n.to_string()).collect();
        assert_eq!(keys, expected, "every row once, in order");
        assert!(fixture.hits() >= 3, "{} windows", fixture.hits());
    }
}
