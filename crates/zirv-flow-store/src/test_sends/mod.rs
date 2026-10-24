//! Template test sends and their log (architecture 11.7, #224).
//!
//! A test send writes no `messages` row (supervisor ruling 1d3048b4): its `test_sends` row is the
//! whole record, and the row's id is the idempotency key at the destination. Neither this table
//! nor the allowlist stores an address; both hold the keyed identifier hash the suppressions are
//! keyed by.
//!
//! `log` holds the rows, the rate-limited reservation and the lifecycle deletes (retention and
//! erasure). `allowlist` holds the workspace's allowed test addresses.

mod allowlist;
mod log;

pub use allowlist::{
    AllowlistEntry, add_allowlist_entry, delete_allowlist_entry, is_allowlisted, list_allowlist,
    masked_hint,
};
pub use log::{
    MEMBER_HOURLY_LIMIT, NewTestSend, TEST_SEND_JOB_KIND, TEST_SEND_RETENTION_MONTHS, TestSend,
    TestSendCursor, TestSendOutcome, TestSendReservation, WORKSPACE_HOURLY_LIMIT,
    delete_test_sends_before, delete_test_sends_by_hash, find_test_send, finish_test_send,
    is_test_send, list_test_sends, reserve_test_send,
};
