//! Deletion, erasure and export of a person (architecture sections 7.4 and 16.3).
//!
//! [`delete_person`] owns the list of person-scoped tables. Erasure and export build on it and
//! on the ledger tables of migration 0013.

mod delete;
mod erasure;
mod erasure_calls;
mod export;

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
    BuildOutcome, EXPORT_FORMAT, EXPORT_JOB_KIND, ExportCursor, ExportDownload, ExportStatus,
    PrivacyExport, build_export, download_export, expire_exports, find_privacy_export,
    list_privacy_exports, request_export,
};
