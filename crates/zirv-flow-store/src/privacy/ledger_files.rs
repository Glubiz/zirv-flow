//! The erasure ledger outside the database (architecture section 16.3, OQ-13).
//!
//! `erasure_requests` is the ledger. When `privacy.ledger_dir` is set, every completed entry is
//! also written as one JSON file, so an erasure survives a restore of a backup that predates it:
//! [`merge_ledger_entries`] puts the files' entries back into the table as completed and the
//! existing replay does the rest. A file holds ids, keyed hashes (hex) and times, never an
//! identifier value, and its hashes mean something only with the same HMAC root.

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use hashbrown::HashSet;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::{PgPool, StoreError, begin, commit};

/// The `format` of a ledger file.
pub const LEDGER_FORMAT: &str = "zirv-flow.erasure-ledger/v1";

const RESTORE_NOTE: &str = "The hashes are keyed with the deployment HMAC root. Restoring from \
    this file needs the master key and the database's deployment_keys row that were current when \
    the erasure ran.";

/// Entries read per page when exporting.
const EXPORT_PAGE: i64 = 500;

fn restore_note() -> String {
    RESTORE_NOTE.to_owned()
}

/// One completed ledger entry as a file holds it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerEntry {
    pub format: String,
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub person_id: Uuid,
    pub person_ids: Vec<Uuid>,
    /// Lowercase hex of each 32-byte hash, in the order of `identifier_kinds`.
    pub identifier_hashes: Vec<String>,
    pub identifier_kinds: Vec<String>,
    #[serde(default)]
    pub requested_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub requested_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub completed_at: OffsetDateTime,
    pub trace_id: Uuid,
    #[serde(default = "restore_note")]
    pub restore_note: String,
}

impl LedgerEntry {
    /// The stable file name: `<workspace_id>-<ledger_id>.json`.
    pub fn file_name(&self) -> String {
        format!("{}-{}.json", self.workspace_id, self.id)
    }

    /// The exact bytes of the file: pretty JSON and a newline.
    fn to_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    /// The hashes as bytes; an entry whose hashes are not 32-byte hex is corrupt.
    fn hash_bytes(&self) -> Option<Vec<Vec<u8>>> {
        self.identifier_hashes
            .iter()
            .map(|text| decode_hash(text))
            .collect()
    }
}

impl LedgerEntry {
    /// Whether both hold the same person ids, hashes and kinds in the same order.
    pub fn same_lists(&self, other: &Self) -> bool {
        self.person_ids == other.person_ids
            && self.identifier_hashes == other.identifier_hashes
            && self.identifier_kinds == other.identifier_kinds
    }

    /// This entry with whatever `other` adds: its person ids, and its `(hash, kind)` pairs. The
    /// order of this entry's lists is kept and new items are appended.
    fn union(&self, other: &Self) -> Self {
        let mut merged = self.clone();
        for id in &other.person_ids {
            if !merged.person_ids.contains(id) {
                merged.person_ids.push(*id);
            }
        }
        for (hash, kind) in other.identifier_hashes.iter().zip(&other.identifier_kinds) {
            let known = merged
                .identifier_hashes
                .iter()
                .zip(&merged.identifier_kinds)
                .any(|(have, have_kind)| have == hash && have_kind == kind);
            if !known {
                merged.identifier_hashes.push(hash.clone());
                merged.identifier_kinds.push(kind.clone());
            }
        }
        merged
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

fn decode_hash(text: &str) -> Option<Vec<u8>> {
    if text.len() != 64 || !text.is_ascii() {
        return None;
    }
    (0..32)
        .map(|index| u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok())
        .collect()
}

/// Why a ledger file or directory could not be used. Names a file or a directory, never a value.
#[derive(Debug)]
pub enum LedgerFileError {
    Io {
        path: PathBuf,
        action: &'static str,
        source: io::Error,
    },
    /// The file is not a ledger entry.
    Corrupt { file: String },
    /// The file declares another format than [`LEDGER_FORMAT`].
    UnknownFormat { file: String },
    /// A file of that name exists with other content.
    Conflict { file: String },
}

impl LedgerFileError {
    fn io(path: &Path, action: &'static str, source: io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            action,
            source,
        }
    }
}

impl fmt::Display for LedgerFileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                path,
                action,
                source,
            } => write!(formatter, "could not {action} {}: {source}", path.display()),
            Self::Corrupt { file } => write!(formatter, "{file} is not a valid ledger file"),
            Self::UnknownFormat { file } => {
                write!(formatter, "{file} has a format this version does not know")
            }
            Self::Conflict { file } => {
                write!(formatter, "{file} already exists with different content")
            }
        }
    }
}

impl std::error::Error for LedgerFileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// What [`write_ledger_file`] did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LedgerWrite {
    Created,
    /// The file was there with the same content.
    Unchanged,
}

/// Writes the entry's file into `dir`: a temp name in the same directory, fsync, rename into
/// place, fsync of the directory. A file that exists with the same bytes is a success; one with
/// other bytes is [`LedgerFileError::Conflict`]. No partial file is ever visible under the
/// final name.
pub fn write_ledger_file(dir: &Path, entry: &LedgerEntry) -> Result<LedgerWrite, LedgerFileError> {
    let name = entry.file_name();
    let path = dir.join(&name);
    let bytes = entry
        .to_bytes()
        .map_err(|error| LedgerFileError::io(&path, "encode", io::Error::other(error)))?;
    match fs::read(&path) {
        Ok(existing) if existing == bytes || same_but_for_completion(&existing, entry) => {
            return Ok(LedgerWrite::Unchanged);
        }
        Ok(_) => return Err(LedgerFileError::Conflict { file: name }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(LedgerFileError::io(&path, "read", error)),
    }
    durable_write(dir, &name, &bytes)?;
    Ok(LedgerWrite::Created)
}

/// Brings the file of `entry` in `dir` to hold everything both know, and returns what it holds.
///
/// With no file, it writes the entry. With a valid file of the same ledger id and workspace, the
/// result is the union of the two (the file's order first, new items appended): a retry whose
/// person gained an identity, or a pending row restored from an older backup, never shrinks the
/// file. The file is rewritten, by the temp, fsync and rename path, only when the union adds
/// something to it. The caller brings its database row to the returned lists. A file that is
/// unreadable, corrupt, of another format, or named for another ledger id or workspace is an error
/// naming it.
pub fn reconcile_ledger_file(
    dir: &Path,
    entry: &LedgerEntry,
) -> Result<LedgerEntry, LedgerFileError> {
    let name = entry.file_name();
    let path = dir.join(&name);
    let existing = match fs::read(&path) {
        Ok(bytes) => parse_ledger_bytes(&bytes, name.clone())?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            write_ledger_file(dir, entry)?;
            return Ok(entry.clone());
        }
        Err(error) => return Err(LedgerFileError::io(&path, "read", error)),
    };
    let mut merged = existing.union(entry);
    if merged.same_lists(&existing) {
        return Ok(merged);
    }
    merged.completed_at = entry.completed_at;
    let bytes = merged
        .to_bytes()
        .map_err(|error| LedgerFileError::io(&path, "encode", io::Error::other(error)))?;
    durable_write(dir, &name, &bytes)?;
    Ok(merged)
}

/// Whether the bytes are this entry written by an earlier attempt: a retry after a crash between
/// the rename and the commit completes the request again, at a later `completed_at`.
fn same_but_for_completion(existing: &[u8], entry: &LedgerEntry) -> bool {
    serde_json::from_slice::<LedgerEntry>(existing).is_ok_and(|mut earlier| {
        earlier.completed_at = entry.completed_at;
        earlier == *entry
    })
}

fn durable_write(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), LedgerFileError> {
    let temp = dir.join(format!(".{name}.{}.tmp", Uuid::now_v7()));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        // Ledger files name people by keyed hash: owner only.
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options
            .open(&temp)
            .map_err(|error| LedgerFileError::io(&temp, "create", error))?;
        file.write_all(bytes)
            .map_err(|error| LedgerFileError::io(&temp, "write", error))?;
        file.sync_all()
            .map_err(|error| LedgerFileError::io(&temp, "sync", error))?;
        drop(file);
        let path = dir.join(name);
        fs::rename(&temp, &path).map_err(|error| LedgerFileError::io(&path, "rename into", error))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
        return result;
    }
    sync_directory(dir)
}

/// Makes the rename durable. Windows cannot open a directory as a file, so there the rename
/// rests on the file system's own journal.
#[cfg(unix)]
fn sync_directory(dir: &Path) -> Result<(), LedgerFileError> {
    fs::File::open(dir)
        .and_then(|handle| handle.sync_all())
        .map_err(|error| LedgerFileError::io(dir, "sync", error))
}

#[cfg(not(unix))]
fn sync_directory(_dir: &Path) -> Result<(), LedgerFileError> {
    Ok(())
}

/// The start-up probe: writes, fsyncs, renames and removes a file in `dir`. The directory must
/// exist.
pub fn check_ledger_dir(dir: &Path) -> Result<(), LedgerFileError> {
    let metadata = fs::metadata(dir).map_err(|error| LedgerFileError::io(dir, "open", error))?;
    if !metadata.is_dir() {
        return Err(LedgerFileError::io(
            dir,
            "use as a directory",
            io::Error::other("not a directory"),
        ));
    }
    let name = format!(".probe-{}", Uuid::now_v7());
    let probe = dir.join(&name);
    // The rename may succeed and a later step fail: the probe goes in every case, and the first
    // error is the one reported.
    let written = durable_write(dir, &name, b"probe");
    let removed = fs::remove_file(&probe);
    written?;
    removed.map_err(|error| LedgerFileError::io(&probe, "remove", error))
}

/// Reads and validates every ledger file of `dir`, sorted by file name. Files whose name does not
/// end in `.json` (and the hidden temp files of a write in progress) are ignored. One file that
/// is corrupt, of an unknown format or named other than its entry fails the whole read, naming
/// that file.
pub fn read_ledger_dir(dir: &Path) -> Result<Vec<LedgerEntry>, LedgerFileError> {
    read_ledger_dir_each(dir)?.into_iter().collect()
}

/// Like [`read_ledger_dir`], but a file that cannot be used does not stop the others: each file
/// gives its own result, in file-name order. Only a directory that cannot be listed fails.
pub fn read_ledger_dir_each(
    dir: &Path,
) -> Result<Vec<Result<LedgerEntry, LedgerFileError>>, LedgerFileError> {
    let listing = fs::read_dir(dir).map_err(|error| LedgerFileError::io(dir, "read", error))?;
    let mut names: Vec<String> = Vec::new();
    for item in listing {
        let item = item.map_err(|error| LedgerFileError::io(dir, "read", error))?;
        let Ok(name) = item.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') || !name.ends_with(".json") {
            continue;
        }
        names.push(name);
    }
    names.sort();
    Ok(names
        .into_iter()
        .map(|name| read_ledger_file(dir, name))
        .collect())
}

/// Reads one `*.json` entry, following a symlink. Anything that is not a regular file after that
/// (a directory, a dangling link) is corrupt.
fn read_ledger_file(dir: &Path, name: String) -> Result<LedgerEntry, LedgerFileError> {
    let path = dir.join(&name);
    if !fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
        return Err(LedgerFileError::Corrupt { file: name });
    }
    let bytes = fs::read(&path).map_err(|error| LedgerFileError::io(&path, "read", error))?;
    parse_ledger_bytes(&bytes, name)
}

/// Validates the bytes of the file called `name`.
fn parse_ledger_bytes(bytes: &[u8], name: String) -> Result<LedgerEntry, LedgerFileError> {
    let corrupt = || LedgerFileError::Corrupt { file: name.clone() };
    let value: Value = serde_json::from_slice(bytes).map_err(|_| corrupt())?;
    match value.get("format").and_then(Value::as_str) {
        Some(LEDGER_FORMAT) => {}
        Some(_) => return Err(LedgerFileError::UnknownFormat { file: name }),
        None => return Err(corrupt()),
    }
    let entry: LedgerEntry = serde_json::from_value(value).map_err(|_| corrupt())?;
    let distinct = {
        let pairs: HashSet<(&String, &String)> = entry
            .identifier_hashes
            .iter()
            .zip(&entry.identifier_kinds)
            .collect();
        let people: HashSet<&Uuid> = entry.person_ids.iter().collect();
        pairs.len() == entry.identifier_hashes.len() && people.len() == entry.person_ids.len()
    };
    let consistent = entry.file_name() == name
        && entry.identifier_hashes.len() == entry.identifier_kinds.len()
        && entry.hash_bytes().is_some()
        && distinct;
    if consistent {
        Ok(entry)
    } else {
        Err(corrupt())
    }
}

type ExportRow = (
    Uuid,
    Uuid,
    Uuid,
    Vec<Uuid>,
    Vec<Vec<u8>>,
    Vec<String>,
    Option<Uuid>,
    OffsetDateTime,
    Option<OffsetDateTime>,
    Uuid,
);

fn entry_of(row: ExportRow) -> LedgerEntry {
    let (id, workspace_id, person_id, person_ids, hashes, kinds, by, requested, completed, trace) =
        row;
    LedgerEntry {
        format: LEDGER_FORMAT.to_owned(),
        id,
        workspace_id,
        person_id,
        person_ids,
        identifier_hashes: hashes.iter().map(|hash| encode_hex(hash)).collect(),
        identifier_kinds: kinds,
        requested_by: by,
        requested_at: requested,
        completed_at: completed.unwrap_or(requested),
        trace_id: trace,
        restore_note: restore_note(),
    }
}

const ENTRY_COLUMNS: &str = "id, workspace_id, person_id, person_ids, identifier_hashes, \
    identifier_kinds, requested_by, requested_at, completed_at, trace_id";

/// The completed ledger entry, in the caller's transaction; `None` when the workspace has no
/// such entry or it is not completed.
pub async fn find_ledger_entry(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<LedgerEntry>, StoreError> {
    let row: Option<ExportRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ENTRY_COLUMNS} FROM erasure_requests \
         WHERE workspace_id = $1 AND id = $2 AND status = 'completed'"
    )))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(row.map(entry_of))
}

/// Brings the lists of a completed ledger row to those of `entry` (the person id stays), in the
/// caller's transaction.
pub async fn update_ledger_lists(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    entry: &LedgerEntry,
) -> Result<(), StoreError> {
    let hashes = entry.hash_bytes().ok_or_else(|| {
        StoreError::query(sqlx::Error::Decode("a ledger hash is not 32 bytes".into()))
    })?;
    sqlx::query(
        "UPDATE erasure_requests SET identifier_hashes = $3, identifier_kinds = $4, \
            person_ids = $5 \
         WHERE workspace_id = $1 AND id = $2 AND status = 'completed'",
    )
    .bind(workspace.as_uuid())
    .bind(entry.id)
    .bind(&hashes)
    .bind(&entry.identifier_kinds)
    .bind(&entry.person_ids)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// What [`export_ledger`] wrote, as counts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExportReport {
    /// Files written.
    pub written: u64,
    /// Files that were there with the same content.
    pub unchanged: u64,
}

/// Why [`export_ledger`] stopped.
#[derive(Debug)]
pub enum ExportError {
    Store(StoreError),
    File(LedgerFileError),
}

impl fmt::Display for ExportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => error.fmt(formatter),
            Self::File(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ExportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::File(error) => Some(error),
        }
    }
}

/// Writes a file into `dir` for every completed ledger entry of the database.
pub async fn export_ledger(pool: &PgPool, dir: &Path) -> Result<ExportReport, ExportError> {
    let mut report = ExportReport::default();
    let mut after: Option<(Uuid, Uuid)> = None;
    loop {
        let rows: Vec<ExportRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {ENTRY_COLUMNS} FROM erasure_requests \
             WHERE status = 'completed' \
               AND ($1::uuid IS NULL OR (workspace_id, id) > ($1, $2)) \
             ORDER BY workspace_id, id LIMIT $3"
        )))
        .bind(after.map(|(workspace, _)| workspace))
        .bind(after.map(|(_, id)| id))
        .bind(EXPORT_PAGE)
        .fetch_all(pool)
        .await
        .map_err(|error| ExportError::Store(StoreError::query(error)))?;
        let Some(last) = rows.last() else {
            return Ok(report);
        };
        after = Some((last.1, last.0));
        for row in rows {
            match write_ledger_file(dir, &entry_of(row)).map_err(ExportError::File)? {
                LedgerWrite::Created => report.written += 1,
                LedgerWrite::Unchanged => report.unchanged += 1,
            }
        }
    }
}

/// What [`merge_ledger_entries`] did, as counts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MergeReport {
    /// Entries inserted as completed, and pending or failed rows upgraded to completed.
    pub merged: u64,
    /// Entries whose workspace does not exist here.
    pub skipped_workspaces: u64,
}

/// The guarded upsert of one entry: inserts it, or upgrades a pending or failed row of the same
/// workspace. Returns the rows it changed. When it changes none, the row that is there must not
/// belong to another workspace (an insert that raced the preflight): that is
/// [`MergeError::IdInOtherWorkspace`], and the caller's transaction rolls back.
async fn upsert_entry(
    connection: &mut PgConnection,
    entry: &LedgerEntry,
) -> Result<u64, MergeError> {
    let hashes = entry.hash_bytes().ok_or_else(|| {
        StoreError::query(sqlx::Error::Decode("a ledger hash is not 32 bytes".into()))
    })?;
    let inserted = sqlx::query(
        "INSERT INTO erasure_requests \
            (id, workspace_id, person_id, identifier_hashes, identifier_kinds, person_ids, \
             status, requested_by, requested_at, completed_at, trace_id) \
         VALUES ($1, $2, $3, $4, $5, $6, 'completed', $7, $8, $9, $10) \
         ON CONFLICT (id) DO UPDATE SET \
            status = 'completed', person_id = EXCLUDED.person_id, \
            person_ids = EXCLUDED.person_ids, \
            identifier_hashes = EXCLUDED.identifier_hashes, \
            identifier_kinds = EXCLUDED.identifier_kinds, \
            completed_at = EXCLUDED.completed_at \
         WHERE erasure_requests.status <> 'completed' \
           AND erasure_requests.workspace_id = EXCLUDED.workspace_id",
    )
    .bind(entry.id)
    .bind(entry.workspace_id)
    .bind(entry.person_id)
    .bind(&hashes)
    .bind(&entry.identifier_kinds)
    .bind(&entry.person_ids)
    .bind(entry.requested_by)
    .bind(entry.requested_at)
    .bind(entry.completed_at)
    .bind(entry.trace_id)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let changed = inserted.rows_affected();
    if changed == 0 {
        let holder: Option<Uuid> =
            sqlx::query_scalar("SELECT workspace_id FROM erasure_requests WHERE id = $1")
                .bind(entry.id)
                .fetch_optional(&mut *connection)
                .await
                .map_err(StoreError::query)?;
        if holder.is_some_and(|workspace| workspace != entry.workspace_id) {
            return Err(MergeError::IdInOtherWorkspace {
                file: entry.file_name(),
            });
        }
    }
    Ok(changed)
}

/// Why [`merge_ledger_entries`] changed nothing.
#[derive(Debug)]
pub enum MergeError {
    Store(StoreError),
    /// The file's ledger id belongs to a request of another workspace in this database.
    IdInOtherWorkspace {
        file: String,
    },
}

impl From<StoreError> for MergeError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl fmt::Display for MergeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => error.fmt(formatter),
            Self::IdInOtherWorkspace { file } => write!(
                formatter,
                "the ledger id of {file} belongs to another workspace in this database"
            ),
        }
    }
}

impl std::error::Error for MergeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::IdInOtherWorkspace { .. } => None,
        }
    }
}

/// Inserts the entries the table lacks as `completed`, by ledger id, in one transaction. A row
/// that is still `pending` or `failed` (a backup older than the completion) is upgraded to
/// `completed` with the file's person ids, hashes and completion time, because the file records
/// a completed erasure; both count as `merged`. A `completed` row is left as it is; an entry for a
/// workspace that does not exist is skipped and counted. Idempotent.
pub async fn merge_ledger_entries(
    pool: &PgPool,
    entries: &[LedgerEntry],
) -> Result<MergeReport, MergeError> {
    let mut report = MergeReport::default();
    let mut tx = begin(pool).await?;
    let wanted: Vec<Uuid> = entries.iter().map(|entry| entry.workspace_id).collect();
    let known: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM workspaces WHERE id = ANY($1)")
        .bind(&wanted)
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::query)?;
    let known: HashSet<Uuid> = known.into_iter().collect();
    // Before any change: a ledger id that is taken by another workspace here would be dropped by
    // the guarded upsert, so it stops the merge.
    let ids: Vec<Uuid> = entries.iter().map(|entry| entry.id).collect();
    let taken: Vec<(Uuid, Uuid)> =
        sqlx::query_as("SELECT id, workspace_id FROM erasure_requests WHERE id = ANY($1)")
            .bind(&ids)
            .fetch_all(&mut *tx)
            .await
            .map_err(StoreError::query)?;
    for entry in entries {
        let foreign = taken
            .iter()
            .any(|(id, workspace)| *id == entry.id && *workspace != entry.workspace_id);
        if foreign && known.contains(&entry.workspace_id) {
            return Err(MergeError::IdInOtherWorkspace {
                file: entry.file_name(),
            });
        }
    }
    for entry in entries {
        if !known.contains(&entry.workspace_id) {
            report.skipped_workspaces += 1;
            continue;
        }
        report.merged += upsert_entry(&mut tx, entry).await?;
    }
    commit(tx).await?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("zirv-flow-ledger-{}", Uuid::now_v7()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn entry() -> LedgerEntry {
        LedgerEntry {
            format: LEDGER_FORMAT.to_owned(),
            id: Uuid::now_v7(),
            workspace_id: Uuid::now_v7(),
            person_id: Uuid::now_v7(),
            person_ids: vec![Uuid::now_v7()],
            identifier_hashes: vec![encode_hex(&[7; 32]), encode_hex(&[9; 32])],
            identifier_kinds: vec!["email".to_owned(), "phone".to_owned()],
            requested_by: None,
            requested_at: datetime!(2026-03-01 10:00:00.123456 UTC),
            completed_at: datetime!(2026-03-01 10:00:05 UTC),
            trace_id: Uuid::now_v7(),
            restore_note: restore_note(),
        }
    }

    #[test]
    fn the_file_is_named_from_the_workspace_and_the_entry() {
        let entry = entry();

        assert_eq!(
            entry.file_name(),
            format!("{}-{}.json", entry.workspace_id, entry.id)
        );
        assert_eq!(entry.file_name(), entry.file_name());
    }

    #[test]
    fn a_written_file_reads_back_and_leaves_no_temp_file() {
        let dir = TempDir::new();
        let entry = entry();

        let outcome = write_ledger_file(&dir.0, &entry).unwrap();

        assert_eq!(outcome, LedgerWrite::Created);
        assert_eq!(
            read_ledger_dir(&dir.0).unwrap(),
            std::slice::from_ref(&entry)
        );
        let names: Vec<_> = fs::read_dir(&dir.0)
            .unwrap()
            .map(|item| item.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, [entry.file_name()]);
        let text = fs::read_to_string(dir.0.join(entry.file_name())).unwrap();
        assert!(text.contains(LEDGER_FORMAT) && text.contains("2026-03-01T10:00:05Z"));
        assert!(text.contains("master key"));
    }

    #[test]
    fn reading_each_file_keeps_the_good_entries_beside_a_bad_one() {
        let dir = TempDir::new();
        let good = entry();
        write_ledger_file(&dir.0, &good).unwrap();
        fs::write(dir.0.join("broken.json"), b"{ not json").unwrap();

        let each = read_ledger_dir_each(&dir.0).unwrap();

        assert_eq!(each.len(), 2);
        assert_eq!(*each[0].as_ref().unwrap(), good);
        assert!(matches!(
            &each[1],
            Err(LedgerFileError::Corrupt { file }) if file == "broken.json"
        ));
        assert!(read_ledger_dir(&dir.0).is_err());
    }

    #[test]
    fn a_temp_file_is_never_read_as_an_entry() {
        let dir = TempDir::new();
        let entry = entry();
        fs::write(dir.0.join(format!(".{}.x.tmp", entry.file_name())), b"{").unwrap();
        fs::write(dir.0.join("notes.txt"), b"not json").unwrap();

        assert!(read_ledger_dir(&dir.0).unwrap().is_empty());
    }

    #[test]
    fn writing_the_same_entry_again_is_a_success_and_changes_nothing() {
        let dir = TempDir::new();
        let entry = entry();
        write_ledger_file(&dir.0, &entry).unwrap();
        let before = fs::read(dir.0.join(entry.file_name())).unwrap();

        let outcome = write_ledger_file(&dir.0, &entry).unwrap();

        assert_eq!(outcome, LedgerWrite::Unchanged);
        assert_eq!(fs::read(dir.0.join(entry.file_name())).unwrap(), before);
    }

    #[test]
    fn a_rewrite_at_a_later_completion_time_is_the_same_entry() {
        let dir = TempDir::new();
        let entry = entry();
        write_ledger_file(&dir.0, &entry).unwrap();
        let mut later = entry.clone();
        later.completed_at = datetime!(2026-03-01 10:09:00 UTC);

        assert_eq!(
            write_ledger_file(&dir.0, &later).unwrap(),
            LedgerWrite::Unchanged
        );
        assert_eq!(read_ledger_dir(&dir.0).unwrap(), [entry]);
    }

    #[test]
    fn a_file_with_other_content_is_an_error_and_is_left_alone() {
        let dir = TempDir::new();
        let entry = entry();
        write_ledger_file(&dir.0, &entry).unwrap();
        let mut other = entry.clone();
        other.identifier_hashes = vec![encode_hex(&[1; 32])];
        other.identifier_kinds = vec!["email".to_owned()];
        let before = fs::read(dir.0.join(entry.file_name())).unwrap();

        let error = write_ledger_file(&dir.0, &other).unwrap_err();

        assert!(matches!(error, LedgerFileError::Conflict { .. }), "{error}");
        assert_eq!(fs::read(dir.0.join(entry.file_name())).unwrap(), before);
    }

    #[test]
    fn reconciling_with_no_file_writes_the_entry() {
        let dir = TempDir::new();
        let entry = entry();

        let held = reconcile_ledger_file(&dir.0, &entry).unwrap();

        assert_eq!(held, entry);
        assert_eq!(
            read_ledger_dir(&dir.0).unwrap(),
            std::slice::from_ref(&entry)
        );
    }

    #[test]
    fn reconciling_with_another_view_keeps_the_files_order_and_appends_what_is_new() {
        let dir = TempDir::new();
        let on_disk = entry();
        write_ledger_file(&dir.0, &on_disk).unwrap();
        let mut newer = on_disk.clone();
        newer.identifier_hashes = vec![encode_hex(&[9; 32]), encode_hex(&[5; 32])];
        newer.identifier_kinds = vec!["phone".to_owned(), "email".to_owned()];
        let extra_person = Uuid::now_v7();
        newer.person_ids = vec![extra_person];
        newer.completed_at = datetime!(2026-03-02 10:00:00 UTC);

        let held = reconcile_ledger_file(&dir.0, &newer).unwrap();

        // The pair (9, phone) is known; (5, email) is new; (9, email) would be a different pair.
        assert_eq!(
            held.identifier_hashes,
            [
                encode_hex(&[7; 32]),
                encode_hex(&[9; 32]),
                encode_hex(&[5; 32])
            ]
        );
        assert_eq!(held.identifier_kinds, ["email", "phone", "email"]);
        assert_eq!(held.person_ids, [on_disk.person_ids[0], extra_person]);
        assert_eq!(read_ledger_dir(&dir.0).unwrap(), [held]);
    }

    #[test]
    fn reconciling_with_a_subset_leaves_the_file_as_it_is() {
        let dir = TempDir::new();
        let on_disk = entry();
        write_ledger_file(&dir.0, &on_disk).unwrap();
        let before = fs::read(dir.0.join(on_disk.file_name())).unwrap();
        let mut older = on_disk.clone();
        older.identifier_hashes.truncate(1);
        older.identifier_kinds.truncate(1);
        older.completed_at = datetime!(2026-04-01 00:00:00 UTC);

        let held = reconcile_ledger_file(&dir.0, &older).unwrap();

        assert_eq!(held, on_disk);
        assert_eq!(fs::read(dir.0.join(on_disk.file_name())).unwrap(), before);
    }

    #[test]
    fn reconciling_with_a_corrupt_or_foreign_file_is_an_error_naming_it() {
        let dir = TempDir::new();
        let entry = entry();
        fs::write(dir.0.join(entry.file_name()), b"{ not json").unwrap();
        assert!(matches!(
            reconcile_ledger_file(&dir.0, &entry).unwrap_err(),
            LedgerFileError::Corrupt { file } if file == entry.file_name()
        ));

        // A file under this name that says it belongs to another ledger id.
        let mut foreign = entry.clone();
        foreign.id = Uuid::now_v7();
        fs::write(
            dir.0.join(entry.file_name()),
            serde_json::to_vec(&foreign).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            reconcile_ledger_file(&dir.0, &entry).unwrap_err(),
            LedgerFileError::Corrupt { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn ledger_files_and_the_probe_are_created_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new();
        let entry = entry();

        write_ledger_file(&dir.0, &entry).unwrap();

        let mode = fs::metadata(dir.0.join(entry.file_name()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_json_directory_is_corrupt_and_a_dangling_name_is_not_skipped() {
        let dir = TempDir::new();
        fs::create_dir(dir.0.join("folder.json")).unwrap();

        let error = read_ledger_dir(&dir.0).unwrap_err();

        assert!(
            matches!(&error, LedgerFileError::Corrupt { file } if file == "folder.json"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_file_is_followed_and_a_dangling_link_is_corrupt() {
        let dir = TempDir::new();
        let elsewhere = TempDir::new();
        let entry = entry();
        write_ledger_file(&elsewhere.0, &entry).unwrap();
        std::os::unix::fs::symlink(
            elsewhere.0.join(entry.file_name()),
            dir.0.join(entry.file_name()),
        )
        .unwrap();
        assert_eq!(read_ledger_dir(&dir.0).unwrap(), [entry]);

        std::os::unix::fs::symlink(elsewhere.0.join("nowhere.json"), dir.0.join("gone.json"))
            .unwrap();
        assert!(matches!(
            read_ledger_dir(&dir.0).unwrap_err(),
            LedgerFileError::Corrupt { file } if file == "gone.json"
        ));
    }

    #[test]
    fn a_missing_directory_fails_the_write_and_leaves_nothing() {
        let dir = TempDir::new();
        let missing = dir.0.join("absent");

        let error = write_ledger_file(&missing, &entry()).unwrap_err();

        assert!(matches!(error, LedgerFileError::Io { .. }), "{error}");
        assert!(!missing.exists());
    }

    #[test]
    fn a_directory_replaced_by_a_file_fails_the_write() {
        let dir = TempDir::new();
        let not_a_dir = dir.0.join("file");
        fs::write(&not_a_dir, b"x").unwrap();

        assert!(write_ledger_file(&not_a_dir, &entry()).is_err());
        assert!(check_ledger_dir(&not_a_dir).is_err());
    }

    #[test]
    fn the_probe_passes_in_a_writable_directory_and_leaves_it_empty() {
        let dir = TempDir::new();

        check_ledger_dir(&dir.0).unwrap();

        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
        assert!(check_ledger_dir(&dir.0.join("absent")).is_err());
    }

    #[test]
    fn a_corrupt_file_fails_the_read_naming_the_file() {
        let dir = TempDir::new();
        let good = entry();
        write_ledger_file(&dir.0, &good).unwrap();
        let bad = entry();
        fs::write(dir.0.join(bad.file_name()), b"{ not json").unwrap();

        let error = read_ledger_dir(&dir.0).unwrap_err();

        assert!(
            matches!(&error, LedgerFileError::Corrupt { file } if *file == bad.file_name()),
            "{error}"
        );
    }

    #[test]
    fn an_unknown_format_fails_the_read_naming_the_file() {
        let dir = TempDir::new();
        let mut future = entry();
        future.format = "zirv-flow.erasure-ledger/v2".to_owned();
        fs::write(
            dir.0.join(future.file_name()),
            serde_json::to_vec(&future).unwrap(),
        )
        .unwrap();

        let error = read_ledger_dir(&dir.0).unwrap_err();

        assert!(
            matches!(&error, LedgerFileError::UnknownFormat { file } if *file == future.file_name()),
            "{error}"
        );
    }

    #[test]
    fn a_file_with_a_repeated_pair_or_person_id_is_corrupt() {
        let dir = TempDir::new();
        let mut pairs = entry();
        pairs.identifier_hashes = vec![encode_hex(&[7; 32]), encode_hex(&[7; 32])];
        pairs.identifier_kinds = vec!["email".to_owned(), "email".to_owned()];
        fs::write(
            dir.0.join(pairs.file_name()),
            serde_json::to_vec(&pairs).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            read_ledger_dir(&dir.0).unwrap_err(),
            LedgerFileError::Corrupt { file } if file == pairs.file_name()
        ));
        assert!(matches!(
            reconcile_ledger_file(&dir.0, &pairs).unwrap_err(),
            LedgerFileError::Corrupt { .. }
        ));

        let mut people = entry();
        let id = Uuid::now_v7();
        people.person_ids = vec![id, id];
        fs::write(
            dir.0.join(people.file_name()),
            serde_json::to_vec(&people).unwrap(),
        )
        .unwrap();
        fs::remove_file(dir.0.join(pairs.file_name())).unwrap();
        assert!(matches!(
            read_ledger_dir(&dir.0).unwrap_err(),
            LedgerFileError::Corrupt { file } if file == people.file_name()
        ));

        // The same hash under two kinds is two pairs, not a repeat.
        let mut kinds = entry();
        kinds.identifier_hashes = vec![encode_hex(&[7; 32]), encode_hex(&[7; 32])];
        kinds.identifier_kinds = vec!["email".to_owned(), "phone".to_owned()];
        let other = TempDir::new();
        write_ledger_file(&other.0, &kinds).unwrap();
        assert_eq!(read_ledger_dir(&other.0).unwrap(), [kinds]);
    }

    #[test]
    fn a_file_with_a_short_hash_or_another_name_is_corrupt() {
        let dir = TempDir::new();
        let mut short = entry();
        short.identifier_hashes = vec!["abcd".to_owned(), "ef".to_owned()];
        fs::write(
            dir.0.join(short.file_name()),
            serde_json::to_vec(&short).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            read_ledger_dir(&dir.0).unwrap_err(),
            LedgerFileError::Corrupt { .. }
        ));
        fs::remove_file(dir.0.join(short.file_name())).unwrap();

        let renamed = entry();
        fs::write(
            dir.0.join("someone-else.json"),
            serde_json::to_vec(&renamed).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            read_ledger_dir(&dir.0).unwrap_err(),
            LedgerFileError::Corrupt { .. }
        ));
    }

    #[test]
    fn hashes_round_trip_through_hex() {
        let hash = vec![0xab_u8; 32];

        assert_eq!(decode_hash(&encode_hex(&hash)), Some(hash));
        assert_eq!(decode_hash("zz"), None);
        assert_eq!(decode_hash(&"é".repeat(32)), None);
    }

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = zirv_flow_core::WorkspaceName::parse(slug).unwrap();
        crate::create_workspace(pool, &name, &zirv_flow_core::Slug::parse(slug).unwrap())
            .await
            .unwrap()
            .id
    }

    async fn rows(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM erasure_requests")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn merging_inserts_the_missing_entries_once_and_a_second_run_inserts_none(pool: PgPool) {
        crate::migrate(&pool).await.unwrap();
        let ws = workspace(&pool, "ours").await;
        let mut first = entry();
        first.workspace_id = ws.as_uuid();
        let mut second = entry();
        second.workspace_id = ws.as_uuid();

        let report = merge_ledger_entries(&pool, &[first.clone(), second.clone()])
            .await
            .unwrap();
        let again = merge_ledger_entries(&pool, &[first.clone(), second])
            .await
            .unwrap();

        assert_eq!(
            report,
            MergeReport {
                merged: 2,
                skipped_workspaces: 0
            }
        );
        assert_eq!(again, MergeReport::default());
        assert_eq!(rows(&pool).await, 2);
        let mut conn = pool.acquire().await.unwrap();
        let stored = find_ledger_entry(&mut conn, ws, first.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored, first);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_entry_present_in_both_is_left_untouched(pool: PgPool) {
        crate::migrate(&pool).await.unwrap();
        let ws = workspace(&pool, "ours").await;
        let mut present = entry();
        present.workspace_id = ws.as_uuid();
        merge_ledger_entries(&pool, &[present.clone()])
            .await
            .unwrap();
        let mut changed = present.clone();
        changed.identifier_hashes = vec![encode_hex(&[1; 32])];
        changed.identifier_kinds = vec!["email".to_owned()];

        let report = merge_ledger_entries(&pool, &[changed]).await.unwrap();

        assert_eq!(report, MergeReport::default());
        let mut conn = pool.acquire().await.unwrap();
        let stored = find_ledger_entry(&mut conn, ws, present.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored, present);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_pending_or_failed_row_is_upgraded_to_completed_by_its_file_and_a_rerun_changes_nothing(
        pool: PgPool,
    ) {
        crate::migrate(&pool).await.unwrap();
        let ws = workspace(&pool, "ours").await;
        let mut pending = entry();
        pending.workspace_id = ws.as_uuid();
        let mut failed = entry();
        failed.workspace_id = ws.as_uuid();
        for (file, status) in [(&pending, "pending"), (&failed, "failed")] {
            sqlx::query(
                "INSERT INTO erasure_requests                     (id, workspace_id, person_id, identifier_hashes, identifier_kinds, status,                      requested_by, requested_at, trace_id)                  VALUES ($1, $2, $3, '{}', '{}', $4, $5, $6, $7)",
            )
            .bind(file.id)
            .bind(file.workspace_id)
            .bind(file.person_id)
            .bind(status)
            .bind(file.requested_by)
            .bind(file.requested_at)
            .bind(file.trace_id)
            .execute(&pool)
            .await
            .unwrap();
        }

        let report = merge_ledger_entries(&pool, &[pending.clone(), failed.clone()])
            .await
            .unwrap();
        let again = merge_ledger_entries(&pool, &[pending.clone(), failed.clone()])
            .await
            .unwrap();

        assert_eq!(
            report,
            MergeReport {
                merged: 2,
                skipped_workspaces: 0
            }
        );
        assert_eq!(again, MergeReport::default());
        assert_eq!(rows(&pool).await, 2);
        let mut conn = pool.acquire().await.unwrap();
        for file in [pending, failed] {
            let stored = find_ledger_entry(&mut conn, ws, file.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(stored, file);
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_ledger_id_held_by_another_workspace_stops_the_merge_before_any_change(pool: PgPool) {
        crate::migrate(&pool).await.unwrap();
        let ours = workspace(&pool, "ours").await;
        let theirs = workspace(&pool, "theirs").await;
        let mut held = entry();
        held.workspace_id = theirs.as_uuid();
        merge_ledger_entries(&pool, std::slice::from_ref(&held))
            .await
            .unwrap();
        let mut fine = entry();
        fine.workspace_id = ours.as_uuid();
        let mut clash = entry();
        clash.id = held.id;
        clash.workspace_id = ours.as_uuid();

        let error = merge_ledger_entries(&pool, &[fine, clash.clone()])
            .await
            .unwrap_err();

        assert!(
            matches!(&error, MergeError::IdInOtherWorkspace { file } if *file == clash.file_name()),
            "{error}"
        );
        assert_eq!(rows(&pool).await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_upsert_that_changes_nothing_because_another_workspace_holds_the_id_is_an_error(
        pool: PgPool,
    ) {
        crate::migrate(&pool).await.unwrap();
        let ours = workspace(&pool, "ours").await;
        let theirs = workspace(&pool, "theirs").await;
        let mut held = entry();
        held.workspace_id = theirs.as_uuid();
        merge_ledger_entries(&pool, std::slice::from_ref(&held))
            .await
            .unwrap();
        let mut clash = held.clone();
        clash.workspace_id = ours.as_uuid();
        let mut conn = pool.acquire().await.unwrap();

        // The preflight is skipped: this is the insert that raced it.
        let error = upsert_entry(&mut conn, &clash).await.unwrap_err();
        let same = upsert_entry(&mut conn, &held).await.unwrap();

        assert!(
            matches!(&error, MergeError::IdInOtherWorkspace { file } if *file == clash.file_name()),
            "{error}"
        );
        assert_eq!(same, 0, "a completed row of the same workspace is a no-op");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_entry_for_a_missing_workspace_is_skipped_and_counted(pool: PgPool) {
        crate::migrate(&pool).await.unwrap();
        let ws = workspace(&pool, "ours").await;
        let mut ours = entry();
        ours.workspace_id = ws.as_uuid();
        let stranger = entry();

        let report = merge_ledger_entries(&pool, &[stranger, ours])
            .await
            .unwrap();

        assert_eq!(
            report,
            MergeReport {
                merged: 1,
                skipped_workspaces: 1
            }
        );
        assert_eq!(rows(&pool).await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_export_writes_a_file_per_completed_entry_and_a_second_run_changes_none(
        pool: PgPool,
    ) {
        crate::migrate(&pool).await.unwrap();
        let ws = workspace(&pool, "ours").await;
        let mut done = entry();
        done.workspace_id = ws.as_uuid();
        merge_ledger_entries(&pool, &[done.clone()]).await.unwrap();
        sqlx::query(
            "INSERT INTO erasure_requests (id, workspace_id, person_id, trace_id) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .bind(Uuid::now_v7())
        .bind(Uuid::now_v7())
        .execute(&pool)
        .await
        .unwrap();
        let dir = TempDir::new();

        let first = export_ledger(&pool, &dir.0).await.unwrap();
        let second = export_ledger(&pool, &dir.0).await.unwrap();

        assert_eq!(
            first,
            ExportReport {
                written: 1,
                unchanged: 0
            }
        );
        assert_eq!(
            second,
            ExportReport {
                written: 0,
                unchanged: 1
            }
        );
        assert_eq!(read_ledger_dir(&dir.0).unwrap(), [done]);
    }
}
