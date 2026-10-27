//! Deletion, erasure and export of a person (architecture sections 7.4 and 16.3).
//!
//! [`delete_person`] owns the list of person-scoped tables. Erasure and export build on it and
//! on the ledger tables of migration 0013.

#[cfg(test)]
mod content_scan;
mod delete;
mod erasure;
mod erasure_calls;
mod export;
mod handled_markers;
mod ledger_files;

#[cfg(test)]
pub(crate) mod testing;

pub use delete::{DeleteReason, delete_person};
pub use erasure::{
    ERASE_JOB_KIND, EraseOutcome, ErasureCursor, ErasureRequest, ErasureStatus, ReplayReport,
    erase_person, find_erasure_request, list_erasure_requests, replay_erasures, request_erasure,
};
pub use erasure_calls::{
    ERASE_CALL_ATTEMPTS, ERASE_CALL_JOB_KIND, EXTERNAL_KEY_FIELD, EraseCallOutcome, EraseTarget,
    ErasureCall, ExternalKey, MISSING_KEY, finish_erasure_call, idempotency_key,
    list_erasure_calls, lock_person_call_shared,
};
pub use export::{
    BuildOutcome, EXPORT_EXCLUDED, EXPORT_FORMAT, EXPORT_JOB_KIND, EXPORT_MAX_BYTES_DEFAULT,
    EXPORT_MAX_BYTES_MAX, EXPORT_MAX_BYTES_MIN, EXPORT_SECTIONS, EXPORT_TOO_LARGE, ExportCursor,
    ExportDownload, ExportStatus, PrivacyExport, build_export, download_export, expire_exports,
    find_privacy_export, list_privacy_exports, request_export,
};

pub use ledger_files::{
    ExportError, ExportReport, LEDGER_FORMAT, LedgerEntry, LedgerFileError, LedgerWrite,
    MergeError, MergeReport, check_ledger_dir, export_ledger, find_ledger_entry,
    merge_ledger_entries, read_ledger_dir, reconcile_ledger_file, update_ledger_lists,
    write_ledger_file,
};

pub(crate) use handled_markers::{delete_and_mark, event_columns, mark_partition};
