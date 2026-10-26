//! Template test sends and their log (architecture 11.7, #224).
//!
//! A test send writes no `messages` row (supervisor ruling 1d3048b4): its `test_sends` row is the
//! whole record, and the row's id is the idempotency key at the destination. No job payload
//! holds an address. The log holds the keyed identifier hash the suppressions are keyed by; the
//! allowlist holds it too, with a masked hint and the address sealed with the keyring.
//!
//! `log` holds the rows, the rate-limited reservation and the lifecycle deletes (retention and
//! erasure). `allowlist` holds the workspace's allowed test addresses.

mod allowlist;
mod log;

pub use allowlist::{
    AllowlistEntry, add_allowlist_entry, delete_allowlist_by_hash, delete_allowlist_entry,
    find_allowlist_entry, list_allowlist, masked_hint, open_allowlist_address,
};
pub use log::{
    ABANDONED, MEMBER_HOURLY_LIMIT, NewTestSend, TEST_SEND_JOB_KIND, TEST_SEND_RETENTION_MONTHS,
    TestSend, TestSendCursor, TestSendOutcome, TestSendReservation, WORKSPACE_HOURLY_LIMIT,
    abandon_orphaned_test_sends, delete_test_sends_before, delete_test_sends_by_hash,
    find_test_send, finish_test_send, is_test_send, list_test_sends, record_test_send_versions,
    reserve_test_send, test_send_may_go,
};
