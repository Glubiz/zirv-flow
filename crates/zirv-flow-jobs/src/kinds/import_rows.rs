//! What the two import kinds share: reading an import's stored file row by row, and turning
//! rows into the parts of a batch.
//!
//! Nothing here logs a cell, an identifier or a trait value.

use hashbrown::HashMap;
use std::collections::VecDeque;

use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::TraceContext;
use zirv_flow_core::csv_file::CsvParser;
use zirv_flow_core::mapping::{self, CsvRow, Header, Mapping, TraitState};
use zirv_flow_store::{
    Import, PgPool, ProfileRecord, RejectReason, RejectedRow, RowDetail, StoreError,
    list_trait_definitions, read_import_chunk,
};

/// Rows per batch, and so per transaction.
pub(super) const BATCH_ROWS: usize = 500;

/// Why the file could not be read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReadError {
    /// The stored file is gone, or a chunk of it is.
    Missing,
    /// The stored file does not parse any more.
    Unreadable,
    Store,
}

impl From<StoreError> for ReadError {
    fn from(_: StoreError) -> Self {
        Self::Store
    }
}

impl ReadError {
    /// The code an import that cannot be read fails with; a store error is the job's to retry.
    pub(super) const fn code(self) -> Option<&'static str> {
        match self {
            Self::Missing => Some("file_missing"),
            Self::Unreadable => Some("file_unreadable"),
            Self::Store => None,
        }
    }
}

/// A data row and its number as a spreadsheet shows it (the header is row 1).
pub(super) type NumberedRow = (i64, Vec<String>);

/// Reads the data rows of an import's file in order, one chunk at a time.
pub(super) struct RowReader<'a> {
    pool: &'a PgPool,
    import: Uuid,
    parser: CsvParser,
    next_chunk: i32,
    ended: bool,
    header_pending: bool,
    ready: VecDeque<(i64, Vec<String>)>,
}

impl<'a> RowReader<'a> {
    pub(super) fn new(pool: &'a PgPool, import: &Import) -> Self {
        Self {
            pool,
            import: import.id,
            parser: CsvParser::new(u8::try_from(import.delimiter).unwrap_or(b',')),
            next_chunk: 0,
            ended: false,
            header_pending: true,
            ready: VecDeque::new(),
        }
    }

    /// The next `limit` data rows; fewer at the end of the file, none after it.
    pub(super) async fn next_rows(&mut self, limit: usize) -> Result<Vec<NumberedRow>, ReadError> {
        while self.ready.len() < limit && !self.ended {
            self.load().await?;
        }
        let take = limit.min(self.ready.len());
        let mut rows = Vec::with_capacity(take);
        for (number, cells) in self.ready.drain(..take) {
            rows.push((number, cells));
        }
        Ok(rows)
    }

    /// Reads past the first `rows` data rows, which a checkpoint already covers.
    pub(super) async fn skip(&mut self, mut rows: i64) -> Result<(), ReadError> {
        while rows > 0 {
            let want = usize::try_from(rows).unwrap_or(usize::MAX).min(BATCH_ROWS);
            let got = self.next_rows(want).await?;
            if got.is_empty() {
                return Err(ReadError::Unreadable);
            }
            rows -= i64::try_from(got.len()).unwrap_or(i64::MAX);
        }
        Ok(())
    }

    /// Feeds the parser the next chunk, or ends it.
    async fn load(&mut self) -> Result<(), ReadError> {
        let chunk = read_import_chunk(self.pool, self.import, self.next_chunk).await?;
        let first = self.next_chunk == 0;
        let ready = &mut self.ready;
        let header_pending = &mut self.header_pending;
        let mut take = |row: u64, record: Vec<String>| {
            if *header_pending {
                *header_pending = false;
            } else {
                ready.push_back((i64::try_from(row).unwrap_or(i64::MAX), record));
            }
        };
        match chunk {
            Some(chunk) => {
                self.next_chunk += 1;
                self.parser
                    .push(&chunk, &mut take)
                    .map_err(|_| ReadError::Unreadable)
            }
            None => {
                self.ended = true;
                self.parser
                    .finish(&mut take)
                    .map_err(|_| ReadError::Unreadable)?;
                // An upload always has a header, so a file without one was deleted.
                if first && self.header_pending {
                    return Err(ReadError::Missing);
                }
                Ok(())
            }
        }
    }
}

/// What a test hooks into a pass: a simulated crash after some batches, and work done "behind
/// the handler's back" just before a batch is committed. Release builds pass the default.
#[derive(Default)]
pub(super) struct Probe {
    pub(super) stop_after: Option<usize>,
    pub(super) before_commit: Option<Box<dyn Fn() -> BoxFuture + Send + Sync>>,
}

pub(super) type BoxFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

impl Probe {
    pub(super) fn stopping_after(batches: Option<usize>) -> Self {
        Self {
            stop_after: batches,
            before_commit: None,
        }
    }

    /// Runs the `before_commit` hook once, if there is one.
    pub(super) async fn before_commit(&self) {
        if let Some(hook) = &self.before_commit {
            hook().await;
        }
    }
}

/// What mapping needs besides the rows.
pub(super) struct Planner<'a> {
    pub(super) mapping: &'a Mapping,
    pub(super) header: &'a Header,
    pub(super) catalog: &'a HashMap<String, TraitState>,
    pub(super) source: &'a str,
    pub(super) stream: &'a str,
    /// Hex of the file's SHA-256, for the record key of a row without a key column value.
    pub(super) file_hash: &'a str,
    pub(super) observed_at: OffsetDateTime,
    pub(super) trace: &'a TraceContext,
}

/// The parts of one batch.
#[derive(Default)]
pub(super) struct Planned {
    pub(super) records: Vec<ProfileRecord>,
    pub(super) details: Vec<RowDetail>,
    pub(super) rejected: Vec<RejectedRow>,
}

impl Planner<'_> {
    /// Maps `rows`: the accepted ones become profile records, the others rejects.
    pub(super) fn plan(&self, rows: Vec<NumberedRow>) -> Planned {
        let mut planned = Planned::default();
        for (number, cells) in rows {
            let row = CsvRow {
                header: self.header,
                cells: &cells,
            };
            match mapping::apply(self.mapping, &row, self.catalog) {
                Ok(mapped) => {
                    planned.records.push(ProfileRecord {
                        source: self.source.to_owned(),
                        stream: self.stream.to_owned(),
                        record_key: mapped
                            .key
                            .unwrap_or_else(|| format!("{}:{number}", self.file_hash)),
                        identifiers: mapped.identifiers,
                        traits: mapped.traits,
                        observed_at: self.observed_at,
                        trace: self.trace.clone(),
                    });
                    planned.details.push(RowDetail {
                        row_number: number,
                        lists: mapped.lists,
                        raw: cells,
                    });
                }
                Err(errors) => planned.rejected.push(RejectedRow {
                    row_number: number,
                    reasons: errors
                        .into_iter()
                        .map(|error| RejectReason {
                            column: error.column,
                            reason: error.reason.as_str(),
                        })
                        .collect(),
                    raw: cells,
                }),
            }
        }
        planned
    }
}

/// The trait registry as the mapping sees it.
pub(super) async fn load_catalog(
    pool: &PgPool,
    workspace: zirv_flow_core::WorkspaceId,
) -> Result<HashMap<String, TraitState>, StoreError> {
    Ok(list_trait_definitions(pool, workspace)
        .await?
        .into_iter()
        .map(|definition| {
            let state = if definition.archived {
                TraitState::Archived
            } else {
                TraitState::Active(definition.trait_type)
            };
            (definition.key, state)
        })
        .collect())
}

/// The lowercase hex of a SHA-256.
pub(super) fn hex(digest: &[u8; 32]) -> String {
    use std::fmt::Write;

    digest
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_digest_is_written_as_lowercase_hex() {
        let mut digest = [0_u8; 32];
        digest[0] = 0xab;
        digest[31] = 0x01;

        let text = hex(&digest);

        assert_eq!(text.len(), 64);
        assert!(text.starts_with("ab00"));
        assert!(text.ends_with("01"));
    }
}
