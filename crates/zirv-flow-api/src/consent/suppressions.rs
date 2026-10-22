//! Identifier suppressions. A suppression is stored under the keyed hash of the identifier;
//! the value is hashed on the way in and never kept, so listings and the export show hashes
//! only.

use hashbrown::HashSet;
use std::io;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use futures_util::stream;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::consent::SuppressionReason;
use zirv_flow_core::people::{Identifier, identifier_hash};
use zirv_flow_core::{HmacKey, ValidationError, WorkspaceId};
use zirv_flow_store::{
    AddOutcome, BatchEntry, NewSuppression, PgPool, RemoveOutcome, SUPPRESSION_KINDS, Suppression,
    SuppressionCursor,
};

use super::{bool_param, csv, hex, origin};
use crate::AppState;
use crate::admin::{self, AdminError, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::jobs_admin::{DEFAULT_LIMIT, decode_cursor, encode_cursor, parse_limit};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

const MAX_NOTE_CHARS: usize = 500;
/// The most data rows an import takes.
const MAX_IMPORT_ROWS: usize = 100_000;
/// Hashes written, or looked up, per statement during an import.
const IMPORT_CHUNK: usize = 1000;
/// Suppressions per page of the export.
pub(super) const EXPORT_PAGE: u32 = 1000;
const EXPORT_HEADER: &str = "hash,kind,reason,source,created_at\n";

#[derive(Serialize, ToSchema)]
pub(crate) struct SuppressionBody {
    /// The first 8 hex characters of the identifier hash. The full hash and the value are
    /// never returned here.
    #[schema(example = "9f2a41c7")]
    hash_prefix: String,
    /// `email` or `phone`.
    #[schema(example = "email")]
    kind: String,
    /// `hard_bounce`, `complaint`, `manual` or `erased`.
    #[schema(example = "manual")]
    reason: String,
    /// Where it came from, such as `api`, `ui`, `import` or `erasure`.
    #[schema(example = "ui")]
    source: String,
    note: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
}

impl From<&Suppression> for SuppressionBody {
    fn from(suppression: &Suppression) -> Self {
        Self {
            hash_prefix: hex(&suppression.identifier_hash[..4]),
            kind: suppression.kind.clone(),
            reason: suppression.reason.as_str().to_owned(),
            source: suppression.source.clone(),
            note: suppression.note.clone(),
            created_at: suppression.created_at,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SuppressionsBody {
    /// Newest first.
    suppressions: Vec<SuppressionBody>,
    /// Pass as `before` for the next page; null when there is none.
    next_before: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct IdentifierRequest {
    /// `email` or `phone`.
    #[schema(example = "email")]
    kind: String,
    /// The address or number. It is hashed and not stored.
    value: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct AddSuppressionRequest {
    /// `email` or `phone`.
    #[schema(example = "email")]
    kind: String,
    /// The address or number. It is hashed and not stored.
    value: String,
    /// At most 500 characters. It must not contain the address or number.
    note: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct AddedBody {
    /// False when the identifier was suppressed already.
    added: bool,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct CheckBody {
    suppressed: bool,
    /// The reason it is suppressed.
    reason: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct InvalidRowBody {
    /// The 1-based line of the file the row starts on, the header being line 1. Blank lines are
    /// skipped but counted, and a line break inside a quoted value counts as a line.
    row: usize,
    /// A stable code, never the value: `malformed_row`, `unsupported_kind`,
    /// `invalid_email`, `invalid_phone`, `note_too_long` or `note_contains_value`.
    reason: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SuppressionImportBody {
    /// True when nothing was written.
    dry_run: bool,
    /// Identifiers that were, or on a dry run would be, newly suppressed.
    added: usize,
    /// Identifiers already suppressed, and repeats within the file.
    existing: usize,
    invalid: Vec<InvalidRowBody>,
}

fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "suppressions are unavailable: the server has no master key",
    )
}

fn identifier_key(state: &AppState) -> Result<&HmacKey, ApiError> {
    state.identifier_key.as_deref().ok_or_else(unavailable)
}

/// The normalized identifier of a suppression, or the code of the rule it broke.
fn parse_identifier(kind: &str, value: &str) -> Result<Identifier, &'static str> {
    if !SUPPRESSION_KINDS.contains(&kind) {
        return Err("unsupported_kind");
    }
    Identifier::parse(kind, value).map_err(|error| error.reason())
}

/// Whether `note` is short enough and does not contain the identifier it is about.
fn check_note(note: &str, raw: &str, identifier: &Identifier) -> Result<(), &'static str> {
    if note.chars().count() > MAX_NOTE_CHARS {
        return Err("note_too_long");
    }
    let note = note.to_lowercase();
    let raw = raw.trim().to_lowercase();
    if note.contains(&raw) || note.contains(&identifier.value().to_lowercase()) {
        return Err("note_contains_value");
    }
    Ok(())
}

fn refused(field: &'static str, code: &'static str) -> AdminError {
    AdminError::Invalid(ValidationError::new(field, code))
}

fn parse_for_request(kind: &str, value: &str) -> Result<Identifier, AdminError> {
    parse_identifier(kind, value).map_err(|code| {
        let field = if code == "unsupported_kind" {
            "kind"
        } else {
            "value"
        };
        refused(field, code)
    })
}

/// Suppresses one identifier as `manual` and audits it. `Existing` when it was suppressed
/// already: nothing changes and nothing is audited.
pub(crate) async fn add_manual(
    pool: &PgPool,
    key: &HmacKey,
    context: &Context,
    workspace: WorkspaceId,
    request: &AddSuppressionRequest,
    source: &str,
    created_by: Option<Uuid>,
) -> Result<AddOutcome, AdminError> {
    let identifier = parse_for_request(&request.kind, &request.value)?;
    if let Some(note) = &request.note {
        check_note(note, &request.value, &identifier).map_err(|code| refused("note", code))?;
    }
    let mut tx = zirv_flow_store::begin(pool).await?;
    let outcome = zirv_flow_store::add_suppression(
        &mut *tx,
        workspace,
        &NewSuppression {
            identifier_hash: identifier_hash(key, &identifier),
            kind: &request.kind,
            reason: SuppressionReason::Manual,
            source,
            note: request.note.as_deref(),
            created_by,
        },
    )
    .await?;
    if outcome == AddOutcome::Added {
        admin::audit(
            &mut tx,
            context,
            Some(workspace),
            "suppression.added",
            None,
            json!({ "kind": request.kind, "reason": "manual" }),
        )
        .await?;
    }
    zirv_flow_store::commit(tx).await?;
    Ok(outcome)
}

/// Lifts a suppression and audits it. An `erased` suppression is refused with a conflict.
pub(crate) async fn remove(
    pool: &PgPool,
    key: &HmacKey,
    context: &Context,
    workspace: WorkspaceId,
    request: &IdentifierRequest,
) -> Result<(), AdminError> {
    let identifier = parse_for_request(&request.kind, &request.value)?;
    let mut tx = zirv_flow_store::begin(pool).await?;
    match zirv_flow_store::remove_suppression(
        &mut *tx,
        workspace,
        &identifier_hash(key, &identifier),
    )
    .await?
    {
        RemoveOutcome::Removed => {}
        RemoveOutcome::NotFound => return Err(AdminError::NotFound("suppression")),
        RemoveOutcome::Erased => {
            return Err(AdminError::Conflict(
                "an erased suppression is permanent and cannot be removed",
            ));
        }
    }
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "suppression.removed",
        None,
        json!({ "kind": request.kind }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(())
}

/// What an import found in a file.
struct ParsedImport {
    /// One entry per distinct valid identifier: its hash, kind and note.
    entries: Vec<([u8; 32], String, Option<String>)>,
    /// Valid rows that repeat an identifier earlier in the file.
    repeats: usize,
    invalid: Vec<InvalidRowBody>,
}

impl ParsedImport {
    fn reject(&mut self, row: usize, reason: &str) {
        self.invalid.push(InvalidRowBody {
            row,
            reason: reason.to_owned(),
        });
    }
}

/// Reads the CSV: a header `kind,value` or `kind,value,note`, then one record per
/// identifier. Bad rows are listed by line number and code; the rest are hashed.
fn parse_import(text: &str, key: &HmacKey) -> Result<ParsedImport, AdminError> {
    let mut records = csv::parse(text).into_iter();
    let header: Vec<String> = records
        .next()
        .map(|record| record.fields)
        .unwrap_or_default()
        .into_iter()
        .map(|name| name.trim().to_lowercase())
        .collect();
    if header != ["kind", "value"] && header != ["kind", "value", "note"] {
        return Err(refused("header", "must be kind,value or kind,value,note"));
    }
    let records: Vec<csv::Record> = records.collect();
    if records.len() > MAX_IMPORT_ROWS {
        return Err(refused("rows", "must be at most 100000 rows"));
    }
    let mut parsed = ParsedImport {
        entries: Vec::new(),
        repeats: 0,
        invalid: Vec::new(),
    };
    let mut seen: HashSet<[u8; 32]> = HashSet::new();
    for csv::Record {
        line: row,
        fields: record,
    } in records
    {
        if record.len() < 2 || record.len() > header.len() {
            parsed.reject(row, "malformed_row");
            continue;
        }
        let kind = record[0].trim().to_lowercase();
        let identifier = match parse_identifier(&kind, &record[1]) {
            Ok(identifier) => identifier,
            Err(code) => {
                parsed.reject(row, code);
                continue;
            }
        };
        let note = record
            .get(2)
            .map(|note| note.trim())
            .filter(|note| !note.is_empty());
        if let Some(note) = note
            && let Err(code) = check_note(note, &record[1], &identifier)
        {
            parsed.reject(row, code);
            continue;
        }
        let hash = identifier_hash(key, &identifier);
        if seen.insert(hash) {
            parsed.entries.push((hash, kind, note.map(str::to_owned)));
        } else {
            parsed.repeats += 1;
        }
    }
    Ok(parsed)
}

/// Imports a CSV of identifiers as `manual` suppressions. On a dry run nothing is written;
/// otherwise every distinct valid identifier is added in one transaction and the import is
/// audited with counts only.
pub(crate) async fn import(
    pool: &PgPool,
    key: &HmacKey,
    context: &Context,
    workspace: WorkspaceId,
    text: &str,
    dry_run: bool,
    created_by: Option<Uuid>,
) -> Result<SuppressionImportBody, AdminError> {
    let parsed = parse_import(text, key)?;
    let distinct = parsed.entries.len();
    let added = if dry_run {
        let mut existing = 0;
        for chunk in parsed.entries.chunks(IMPORT_CHUNK) {
            let hashes: Vec<[u8; 32]> = chunk.iter().map(|(hash, _, _)| *hash).collect();
            existing += zirv_flow_store::count_suppressed(pool, workspace, &hashes).await?;
        }
        distinct - usize::try_from(existing).unwrap_or(distinct)
    } else {
        let mut tx = zirv_flow_store::begin(pool).await?;
        let mut added = 0_u64;
        for chunk in parsed.entries.chunks(IMPORT_CHUNK) {
            let batch: Vec<BatchEntry<'_>> = chunk
                .iter()
                .map(|(hash, kind, note)| BatchEntry {
                    identifier_hash: *hash,
                    kind,
                    note: note.as_deref(),
                })
                .collect();
            added += zirv_flow_store::add_suppressions(
                &mut *tx,
                workspace,
                &batch,
                SuppressionReason::Manual,
                "import",
                created_by,
            )
            .await?;
        }
        let added = usize::try_from(added).unwrap_or(distinct);
        admin::audit(
            &mut tx,
            context,
            Some(workspace),
            "suppression.imported",
            None,
            json!({
                "added": added,
                "existing": distinct - added + parsed.repeats,
                "invalid": parsed.invalid.len(),
            }),
        )
        .await?;
        zirv_flow_store::commit(tx).await?;
        added
    };
    Ok(SuppressionImportBody {
        dry_run,
        added,
        existing: distinct - added + parsed.repeats,
        invalid: parsed.invalid,
    })
}

type ListQuery = (u32, Option<SuppressionCursor>, Option<SuppressionReason>);

fn parse_list_query(query: &str) -> Result<ListQuery, ApiError> {
    let (mut limit, mut before, mut reason) = (DEFAULT_LIMIT, None, None);
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "limit" => {
                limit = parse_limit(&value)?;
            }
            "before" => {
                before = Some(
                    decode_cursor(&value)
                        .ok_or_else(|| ApiError::bad_request("before is not a valid cursor"))?,
                );
            }
            "reason" => {
                reason = Some(value.parse().map_err(|_| {
                    ApiError::bad_request("reason must be hard_bounce, complaint, manual or erased")
                })?);
            }
            _ => {}
        }
    }
    Ok((limit, before, reason))
}

/// The suppressions of the workspace, newest first, as hash prefixes. Values and full
/// hashes are never returned.
#[utoipa::path(
    get,
    operation_id = "list_suppressions",
    path = "/api/v1/workspaces/{ws}/suppressions",
    tag = "consent",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("limit" = Option<u32>, Query, description = "Suppressions per page, 1 to 200; default 50"),
        ("before" = Option<String>, Query, description = "The next_before of the previous page"),
        ("reason" = Option<String>, Query, description = "Only this reason: hard_bounce, complaint, manual or erased")
    ),
    responses(
        (status = 200, description = "A page of suppressions", body = SuppressionsBody),
        (status = 400, description = "limit or reason is out of range, or before is malformed", body = Problem, content_type = "application/problem+json"),
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
) -> Result<Json<SuppressionsBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let (limit, before, reason) = parse_list_query(query.as_deref().unwrap_or_default())?;
    let (page, more) =
        zirv_flow_store::list_suppressions(&state.pool, workspace.id, limit, before, reason)
            .await?;
    let next_before = match page.last() {
        Some(last) if more => Some(encode_cursor((last.created_at, last.id))),
        _ => None,
    };
    Ok(Json(SuppressionsBody {
        suppressions: page.iter().map(SuppressionBody::from).collect(),
        next_before,
    }))
}

/// Whether an address or number is suppressed. A POST so the value never sits in a URL.
#[utoipa::path(
    post,
    operation_id = "check_suppression",
    path = "/api/v1/workspaces/{ws}/suppressions/check",
    tag = "consent",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = IdentifierRequest,
    responses(
        (status = 200, description = "Whether it is suppressed, and why", body = CheckBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An unsupported kind or an invalid value", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn check(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<IdentifierRequest>,
) -> Result<Json<CheckBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let key = identifier_key(&state)?;
    let identifier = parse_for_request(&body.kind, &body.value)?;
    let found = zirv_flow_store::find_suppression(
        &state.pool,
        workspace.id,
        &identifier_hash(key, &identifier),
    )
    .await?;
    Ok(Json(CheckBody {
        suppressed: found.is_some(),
        reason: found.map(|found| found.reason.as_str().to_owned()),
    }))
}

/// Suppresses an address or number with the reason `manual`. Audited as
/// `suppression.added`.
#[utoipa::path(
    post,
    operation_id = "add_suppression",
    path = "/api/v1/workspaces/{ws}/suppressions",
    tag = "consent",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = AddSuppressionRequest,
    responses(
        (status = 200, description = "Whether it was newly suppressed", body = AddedBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An unsupported kind, an invalid value, or a note that is too long or holds the value", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn add(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<AddSuppressionRequest>,
) -> Result<Json<AddedBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let key = identifier_key(&state)?;
    let (source, created_by) = origin(&principal);
    let outcome = add_manual(
        &state.pool,
        key,
        &principal.context(),
        workspace.id,
        &body,
        source.as_str(),
        created_by,
    )
    .await?;
    Ok(Json(AddedBody {
        added: outcome == AddOutcome::Added,
    }))
}

/// Lifts a suppression. An `erased` suppression is permanent and answers 409. Audited as
/// `suppression.removed`.
#[utoipa::path(
    post,
    operation_id = "remove_suppression",
    path = "/api/v1/workspaces/{ws}/suppressions/remove",
    tag = "consent",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = IdentifierRequest,
    responses(
        (status = 204, description = "The suppression was lifted"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or suppression, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The suppression is erased and permanent", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An unsupported kind or an invalid value", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn remove_handler(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<IdentifierRequest>,
) -> Result<StatusCode, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let key = identifier_key(&state)?;
    remove(&state.pool, key, &principal.context(), workspace.id, &body).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Imports suppressions from a CSV body (`text/csv`, at most 10 MiB and 100,000 rows) with
/// the header `kind,value` or `kind,value,note`. With `dry_run=true` nothing is written and
/// the answer says what a commit would do. Invalid rows are reported by line number and
/// code, never by value. Audited as `suppression.imported` with counts only.
#[utoipa::path(
    post,
    operation_id = "import_suppressions",
    path = "/api/v1/workspaces/{ws}/suppressions/import",
    tag = "consent",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("dry_run" = Option<bool>, Query, description = "Only report what an import would do; default false")
    ),
    request_body(content = String, content_type = "text/csv", description = "kind,value[,note] rows"),
    responses(
        (status = 200, description = "What was, or on a dry run would be, imported", body = SuppressionImportBody),
        (status = 400, description = "dry_run is not true or false, or the body is not UTF-8", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "The body is over 10 MiB", body = Problem, content_type = "application/problem+json"),
        (status = 415, description = "The content type is not text/csv", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "A wrong header, or more than 100,000 rows", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key", body = Problem, content_type = "application/problem+json")
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
) -> Result<Json<SuppressionImportBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let key = identifier_key(&state)?;
    let dry_run = bool_param(query.as_deref().unwrap_or_default(), "dry_run")?;
    let text = csv::read_body(&headers, body).await?;
    let (_, created_by) = origin(&principal);
    let result = import(
        &state.pool,
        key,
        &principal.context(),
        workspace.id,
        &text,
        dry_run,
        created_by,
    )
    .await?;
    Ok(Json(result))
}

enum Export {
    Header,
    Page(Option<SuppressionCursor>),
    Done,
}

fn export_lines(page: &[Suppression]) -> String {
    let mut text = String::new();
    for suppression in page {
        let created_at = suppression.created_at.format(&Rfc3339).unwrap_or_default();
        let line = [
            hex(&suppression.identifier_hash),
            suppression.kind.clone(),
            suppression.reason.as_str().to_owned(),
            csv::field(&suppression.source),
            created_at,
        ];
        text.push_str(&line.join(","));
        text.push('\n');
    }
    text
}

/// Every suppression as CSV, `hash,kind,reason,source,created_at`, streamed page by page.
/// Values are never stored, so the file holds hashes only.
#[utoipa::path(
    get,
    operation_id = "export_suppressions",
    path = "/api/v1/workspaces/{ws}/suppressions/export",
    tag = "consent",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The suppressions as CSV, newest first", body = String, content_type = "text/csv"),
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
    let body = stream::unfold(Export::Header, move |phase| {
        let pool = pool.clone();
        async move {
            match phase {
                Export::Header => Some((
                    Ok(Bytes::from_static(EXPORT_HEADER.as_bytes())),
                    Export::Page(None),
                )),
                Export::Page(cursor) => {
                    let page = zirv_flow_store::list_suppressions(
                        &pool,
                        workspace,
                        EXPORT_PAGE,
                        cursor,
                        None,
                    )
                    .await;
                    match page {
                        Ok((page, more)) => {
                            let next = match page.last() {
                                Some(last) if more => {
                                    Export::Page(Some((last.created_at, last.id)))
                                }
                                _ => Export::Done,
                            };
                            Some((Ok(Bytes::from(export_lines(&page))), next))
                        }
                        Err(error) => {
                            tracing::error!(%error, "the suppression export failed");
                            Some((Err(io::Error::other("the export failed")), Export::Done))
                        }
                    }
                }
                Export::Done => None,
            }
        }
    });
    Ok(csv::download(Body::from_stream(body), "suppressions.csv"))
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use zirv_flow_core::HmacPurpose;
    use zirv_flow_core::{HmacRoot, MasterKey};

    use super::*;

    fn key() -> HmacKey {
        HmacRoot::bootstrap_from(
            &MasterKey::parse("BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=").unwrap(),
        )
        .hmac_key(HmacPurpose::Identifier)
    }

    fn hash_of(value: &str) -> [u8; 32] {
        identifier_hash(&key(), &Identifier::parse("email", value).unwrap())
    }

    #[test]
    fn only_email_and_phone_can_be_suppressed_and_values_are_normalized() {
        assert_eq!(
            parse_identifier("email", " Ada@Example.COM ")
                .unwrap()
                .value(),
            "ada@example.com"
        );
        assert_eq!(
            parse_identifier("anonymous_id", "x").unwrap_err(),
            "unsupported_kind"
        );
        assert_eq!(
            parse_identifier("email", "nope").unwrap_err(),
            "invalid_email"
        );
        assert_eq!(
            parse_identifier("phone", "123").unwrap_err(),
            "invalid_phone"
        );
    }

    #[test]
    fn a_note_may_not_hold_the_address_in_any_case_or_be_too_long() {
        let identifier = Identifier::parse("email", "Ada@Example.com").unwrap();

        assert!(check_note("asked by phone", "Ada@Example.com", &identifier).is_ok());
        assert_eq!(
            check_note("it is ADA@example.com", "Ada@Example.com", &identifier).unwrap_err(),
            "note_contains_value"
        );
        assert_eq!(
            check_note(&"n".repeat(501), "Ada@Example.com", &identifier).unwrap_err(),
            "note_too_long"
        );
        assert!(check_note(&"n".repeat(500), "Ada@Example.com", &identifier).is_ok());
    }

    #[test]
    fn an_import_hashes_valid_rows_and_reports_the_others_by_number_and_code() {
        let text = "kind,value,note\n\
                    email,Ada@Example.com,asked to stop\n\
                    email,not-an-address,\n\
                    fax,123,\n\
                    email,ada@example.com,\n\
                    phone,+4512345678,\n\
                    email\n\
                    email,bob@example.com,bob@example.com wrote in\n";

        let parsed = parse_import(text, &key()).unwrap();

        let rows: Vec<(usize, &str)> = parsed
            .invalid
            .iter()
            .map(|row| (row.row, row.reason.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                (3, "invalid_email"),
                (4, "unsupported_kind"),
                (7, "malformed_row"),
                (8, "note_contains_value")
            ]
        );
        assert_eq!(parsed.entries.len(), 2);
        assert_eq!(parsed.entries[0].0, hash_of("ada@example.com"));
        assert_eq!(parsed.entries[0].2.as_deref(), Some("asked to stop"));
        assert_eq!(parsed.entries[1].1, "phone");
        assert_eq!(parsed.repeats, 1);
        let shown = format!(
            "{:?}",
            parsed
                .invalid
                .iter()
                .map(|row| &row.reason)
                .collect::<Vec<_>>()
        );
        assert!(!shown.contains('@'));
    }

    #[test]
    fn an_import_needs_the_header_and_takes_two_or_three_columns() {
        for bad in [
            "",
            "value,kind\n",
            "kind\n",
            "kind,value,note,extra\n",
            "email,a@x.com\n",
        ] {
            assert!(
                matches!(
                    parse_import(bad, &key()),
                    Err(AdminError::Invalid(error)) if error.field() == "header"
                ),
                "{bad:?}"
            );
        }
        let two = parse_import("Kind, Value\r\nemail,a@x.com\r\n", &key()).unwrap();
        assert_eq!(two.entries.len(), 1);
        let three_with_two = parse_import("kind,value,note\nemail,a@x.com\n", &key()).unwrap();
        assert_eq!(three_with_two.entries.len(), 1);
        let two_with_three = parse_import("kind,value\nemail,a@x.com,note\n", &key()).unwrap();
        assert_eq!(two_with_three.invalid[0].reason, "malformed_row");
    }

    #[test]
    fn an_import_over_the_row_limit_is_refused() {
        let mut text = String::from("kind,value\n");
        for index in 0..=MAX_IMPORT_ROWS {
            text.push_str(&format!("email,u{index}@example.com\n"));
        }

        assert!(matches!(
            parse_import(&text, &key()),
            Err(AdminError::Invalid(error)) if error.field() == "rows"
        ));
        text.truncate(text.rfind("email,").unwrap());
        assert_eq!(
            parse_import(&text, &key()).unwrap().entries.len(),
            MAX_IMPORT_ROWS
        );
    }

    #[test]
    fn the_listing_cursor_carries_the_id_and_never_the_hash() {
        let at = OffsetDateTime::from_unix_timestamp_nanos(1_700_000_000_000_001_000).unwrap();
        let id = Uuid::now_v7();
        let hash = hash_of("ada@example.com");

        let cursor = encode_cursor((at, id));

        assert_eq!(decode_cursor(&cursor), Some((at, id)));
        let decoded = String::from_utf8(URL_SAFE_NO_PAD.decode(&cursor).unwrap()).unwrap();
        assert!(!decoded.contains(&hex(&hash)) && !cursor.contains(&hex(&hash)));
        assert!(decoded.contains(&id.to_string()));
        assert_eq!(decode_cursor("!!"), None);
    }

    #[test]
    fn rows_are_reported_by_the_line_they_start_on() {
        let text = "kind,value,note

email,nope,
\"email\",\"b@example.com\",\"two
lines\"
email,bad2
fax,1
";

        let parsed = parse_import(text, &key()).unwrap();

        let lines: Vec<(usize, &str)> = parsed
            .invalid
            .iter()
            .map(|row| (row.row, row.reason.as_str()))
            .collect();
        assert_eq!(
            lines,
            [
                (3, "invalid_email"),
                (6, "invalid_email"),
                (7, "unsupported_kind")
            ]
        );
        assert_eq!(parsed.entries.len(), 1);
    }

    #[test]
    fn the_list_query_parses_limit_cursor_and_reason() {
        assert_eq!(parse_list_query("").unwrap(), (50, None, None));
        assert_eq!(
            parse_list_query("reason=erased&limit=7").unwrap(),
            (7, None, Some(SuppressionReason::Erased))
        );
        for bad in ["limit=0", "limit=201", "reason=rude", "before=%21"] {
            assert_eq!(
                parse_list_query(bad).unwrap_err().status(),
                StatusCode::BAD_REQUEST,
                "{bad}"
            );
        }
    }

    #[test]
    fn dry_run_is_true_false_or_absent() {
        assert!(!bool_param("", "dry_run").unwrap());
        assert!(bool_param("dry_run=true", "dry_run").unwrap());
        assert!(bool_param("dry_run=1", "dry_run").is_err());
    }

    #[test]
    fn a_listed_suppression_shows_a_four_byte_prefix_and_never_the_whole_hash() {
        let suppression = Suppression {
            id: Uuid::now_v7(),
            identifier_hash: hash_of("ada@example.com"),
            kind: "email".into(),
            reason: SuppressionReason::Manual,
            source: "ui".into(),
            note: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            created_by: None,
        };

        let json = serde_json::to_string(&SuppressionBody::from(&suppression)).unwrap();

        let full = hex(&suppression.identifier_hash);
        assert!(json.contains(&format!("\"hash_prefix\":\"{}\"", &full[..8])));
        assert!(!json.contains(&full));
    }

    #[test]
    fn an_export_line_holds_the_hash_and_no_value() {
        let suppression = Suppression {
            id: Uuid::now_v7(),
            identifier_hash: [0xab; 32],
            kind: "email".into(),
            reason: SuppressionReason::Erased,
            source: "a,b".into(),
            note: Some("kept out of the export".into()),
            created_at: OffsetDateTime::UNIX_EPOCH,
            created_by: None,
        };

        let lines = export_lines(&[suppression]);

        assert_eq!(
            lines,
            format!(
                "{},email,erased,\"a,b\",1970-01-01T00:00:00Z\n",
                "ab".repeat(32)
            )
        );
    }
}
