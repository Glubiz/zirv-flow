//! One batch of an import, in ONE transaction (architecture sections 7.1 and 7.2): the profile
//! records, the list members, the rejects and the checkpoint commit together or not at all.
//! A crash or a lost lease therefore resumes from `rows_done` and neither skips nor
//! re-applies a row.
//!
//! Nothing here logs a cell, an identifier or a trait value.

use std::collections::BTreeMap;

use uuid::Uuid;
use zirv_flow_core::{TraceContext, WorkspaceId};

use super::model::ImportStatus;
use super::rejects::{ImportPhase, RejectedRow, insert_rejects};
use crate::lists::{ListSource, MAX_PEOPLE_PER_CALL, MembershipError, add_members};
use crate::people::{
    ProfileOutcome, ProfileRecord, apply_profile_records, apply_profile_records_singly,
};
use crate::{PgConnection, PgPool, StoreError, begin, commit};

/// A failed run is judged on its reject rate only once this many rows are done.
pub const REJECT_CHECK_MIN_ROWS: i64 = 1000;

/// What a batch knows about a record beyond the record itself.
#[derive(Clone, Debug)]
pub struct RowDetail {
    /// The row as a spreadsheet numbers it: the header is row 1.
    pub row_number: i64,
    /// The lists the row's person joins.
    pub lists: Vec<Uuid>,
    /// The row's cells, kept if the store refuses the record.
    pub raw: Vec<String>,
}

/// A batch of an import's run.
pub struct ImportBatch<'a> {
    pub import_id: Uuid,
    pub workspace: WorkspaceId,
    /// The checkpoint the batch continues from: the import's `rows_done` before it.
    pub expected_done: i64,
    /// The mapped records, in file order, with `details` in the same order.
    pub records: &'a [ProfileRecord],
    pub details: &'a [RowDetail],
    /// The rows the mapping refused. Together with `records` they are the batch's rows.
    pub rejected: &'a [RejectedRow],
    pub trace: &'a TraceContext,
}

/// Where a committed batch left the import.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BatchProgress {
    pub rows_done: i64,
    pub rows_rejected: i64,
    /// The reject rate went over the import's threshold: the batch is committed and the
    /// import is `failed` with `reject_threshold`.
    pub threshold_exceeded: bool,
}

/// What became of a batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BatchOutcome {
    Committed(BatchProgress),
    /// The import is no longer running at this checkpoint (cancelled, or another attempt got
    /// further): nothing was written.
    Superseded,
    /// A list the mapping names no longer exists: nothing was written.
    ListMissing,
}

/// Counts the outcome of one record and collects the person its lists need.
#[derive(Default)]
struct Tally {
    applied: i64,
    unchanged: i64,
    rejects: Vec<RejectedRow>,
    members: BTreeMap<Uuid, Vec<Uuid>>,
}

impl Tally {
    fn add(&mut self, detail: &RowDetail, result: Option<ProfileOutcome>) {
        let person = match result {
            Some(ProfileOutcome::Applied { person_id, .. }) => {
                self.applied += 1;
                person_id
            }
            Some(ProfileOutcome::Unchanged { person_id, .. }) => {
                self.unchanged += 1;
                person_id
            }
            Some(ProfileOutcome::Rejected { reason }) => {
                return self.reject(detail, reason);
            }
            None => return self.reject(detail, "apply_failed"),
        };
        for list in &detail.lists {
            self.members.entry(*list).or_default().push(person);
        }
    }

    fn reject(&mut self, detail: &RowDetail, reason: &'static str) {
        self.rejects.push(RejectedRow {
            row_number: detail.row_number,
            reasons: vec![super::rejects::RejectReason {
                column: String::new(),
                reason,
            }],
            raw: detail.raw.clone(),
        });
    }
}

/// Applies one batch of the run in a transaction of its own and advances the checkpoint.
///
/// With `singly`, each record is applied in a savepoint of its own and a record the store
/// cannot apply becomes an `apply_failed` reject, so one poison row never stops an import;
/// callers use it after a batch failed with an error that retrying cannot cure. A retryable
/// error rolls the whole batch back either way.
pub async fn commit_import_batch(
    pool: &PgPool,
    batch: &ImportBatch<'_>,
    singly: bool,
) -> Result<BatchOutcome, StoreError> {
    let mut tx = begin(pool).await?;
    crate::changes::cache_change_readers(&mut tx, batch.workspace).await?;
    if !checkpoint_matches(&mut tx, batch, ImportStatus::Running).await? {
        return Ok(BatchOutcome::Superseded);
    }

    let mut tally = Tally::default();
    let outcomes = if singly {
        apply_profile_records_singly(&mut tx, batch.workspace, batch.records).await?
    } else {
        apply_profile_records(&mut tx, batch.workspace, batch.records)
            .await?
            .into_iter()
            .map(Some)
            .collect()
    };
    for (outcome, detail) in outcomes.into_iter().zip(batch.details) {
        tally.add(detail, outcome);
    }

    for (list, people) in &tally.members {
        for chunk in people.chunks(MAX_PEOPLE_PER_CALL) {
            match add_members(
                &mut tx,
                batch.workspace,
                *list,
                chunk,
                ListSource::Import,
                None,
                batch.trace,
            )
            .await
            {
                Ok(_) => {}
                Err(MembershipError::NoSuchList) => return Ok(BatchOutcome::ListMissing),
                Err(MembershipError::TooMany) => return Err(StoreError::poison()),
                Err(MembershipError::Store(error)) => return Err(error),
            }
        }
    }

    let mut rejects = tally.rejects;
    rejects.extend_from_slice(batch.rejected);
    insert_rejects(
        &mut tx,
        batch.workspace,
        batch.import_id,
        ImportPhase::Run,
        &rejects,
    )
    .await?;

    let rows = i64::try_from(batch.records.len() + batch.rejected.len()).unwrap_or(i64::MAX);
    let rejected = i64::try_from(rejects.len()).unwrap_or(i64::MAX);
    let (rows_done, rows_rejected, threshold): (i64, i64, f64) = sqlx::query_as(
        "UPDATE imports SET rows_done = rows_done + $3, rows_applied = rows_applied + $4, \
            rows_unchanged = rows_unchanged + $5, rows_rejected = rows_rejected + $6, \
            updated_at = now() \
         WHERE workspace_id = $1 AND id = $2 \
         RETURNING rows_done, rows_rejected, reject_threshold",
    )
    .bind(batch.workspace.as_uuid())
    .bind(batch.import_id)
    .bind(rows)
    .bind(tally.applied)
    .bind(tally.unchanged)
    .bind(rejected)
    .fetch_one(&mut *tx)
    .await
    .map_err(StoreError::query)?;

    let exceeded = threshold_exceeded(rows_done, rows_rejected, threshold);
    if exceeded {
        sqlx::query(
            "UPDATE imports SET status = 'failed', error_code = 'reject_threshold', \
                finished_at = now() WHERE workspace_id = $1 AND id = $2",
        )
        .bind(batch.workspace.as_uuid())
        .bind(batch.import_id)
        .execute(&mut *tx)
        .await
        .map_err(StoreError::query)?;
    }
    crate::changes::request_reevaluation(&mut tx, batch.workspace).await?;
    commit(tx).await?;
    Ok(BatchOutcome::Committed(BatchProgress {
        rows_done,
        rows_rejected,
        threshold_exceeded: exceeded,
    }))
}

/// Whether the reject rate is over the import's threshold: only once more than
/// [`REJECT_CHECK_MIN_ROWS`] rows are done.
fn threshold_exceeded(rows_done: i64, rows_rejected: i64, threshold: f64) -> bool {
    rows_done > REJECT_CHECK_MIN_ROWS && (rows_rejected as f64) / (rows_done as f64) > threshold
}

/// Locks the import row and says whether it is in `status` at the batch's checkpoint.
async fn checkpoint_matches(
    connection: &mut PgConnection,
    batch: &ImportBatch<'_>,
    status: ImportStatus,
) -> Result<bool, StoreError> {
    let current: Option<(String, i64)> = sqlx::query_as(
        "SELECT status, rows_done FROM imports WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
    )
    .bind(batch.workspace.as_uuid())
    .bind(batch.import_id)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    Ok(
        current
            .is_some_and(|(found, done)| found == status.as_str() && done == batch.expected_done),
    )
}

/// A batch of an import's dry run: nothing is written but the rejects and the counters.
pub struct DryRunBatch<'a> {
    pub import_id: Uuid,
    pub workspace: WorkspaceId,
    pub expected_done: i64,
    /// Rows the mapping accepted.
    pub accepted: i64,
    pub rejected: &'a [RejectedRow],
}

/// Records one batch of the dry run. False when the import is no longer dry running at this
/// checkpoint (it was cancelled): nothing was written.
pub async fn commit_dry_run_batch(
    pool: &PgPool,
    batch: &DryRunBatch<'_>,
) -> Result<bool, StoreError> {
    let mut tx = begin(pool).await?;
    let current: Option<(String, i64)> = sqlx::query_as(
        "SELECT status, rows_done FROM imports WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
    )
    .bind(batch.workspace.as_uuid())
    .bind(batch.import_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(StoreError::query)?;
    let matches = current.is_some_and(|(found, done)| {
        found == ImportStatus::DryRunning.as_str() && done == batch.expected_done
    });
    if !matches {
        return Ok(false);
    }
    insert_rejects(
        &mut tx,
        batch.workspace,
        batch.import_id,
        ImportPhase::DryRun,
        batch.rejected,
    )
    .await?;
    let rejected = i64::try_from(batch.rejected.len()).unwrap_or(i64::MAX);
    sqlx::query(
        "UPDATE imports SET rows_done = rows_done + $3 + $4, rows_applied = rows_applied + $3, \
            rows_rejected = rows_rejected + $4, updated_at = now() \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(batch.workspace.as_uuid())
    .bind(batch.import_id)
    .bind(batch.accepted)
    .bind(rejected)
    .execute(&mut *tx)
    .await
    .map_err(StoreError::query)?;
    commit(tx).await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_threshold_applies_only_after_a_thousand_rows() {
        assert!(!threshold_exceeded(1000, 1000, 0.05));
        assert!(threshold_exceeded(1001, 1001, 0.05));
        assert!(threshold_exceeded(2000, 101, 0.05));
        assert!(!threshold_exceeded(2000, 100, 0.05));
        assert!(!threshold_exceeded(5000, 5000, 1.0));
        assert!(threshold_exceeded(1001, 1, 0.0));
    }
}
