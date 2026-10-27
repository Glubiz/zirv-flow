//! The message log of the send pipeline (architecture 10.1): one `messages` row per message the
//! `delivery.send` job decided on, with the gates it passed, and the address it was sent to read
//! at send time.
//!
//! `messages` is person-scoped. Erasing a person deletes their rows (`privacy::delete`), and on a
//! merge they stay on the id they were written for (see `rewrite_merged_person`); a read of the log
//! goes through `person_merges` ([`list_person_messages`]). A row holds no address.

mod address;
mod log;
mod person_log;
mod retention;

pub use address::read_send_address;
pub(crate) use log::{COLUMNS, insert_message_keeping_gates};
pub use log::{
    GateRecord, Message, MessageStatus, NewMessage, clear_message_in_flight, find_message,
    insert_message, list_person_messages, mark_message_sending, mark_message_unconfirmed,
    message_address_hash, message_in_flight, message_status, message_unconfirmed,
};
pub use person_log::{
    LoggedMessage, MessageLogCursor, MessageLogFilter, MessageLogPage, WaitFilter, WaitReason,
    load_person_message_log,
};
pub use retention::{
    MessageRetention, MessageRetentionReport, apply_message_retention, apply_message_retention_for,
    apply_message_retention_progress,
};
