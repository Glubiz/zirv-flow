//! The message log of the send pipeline (architecture 10.1): one `messages` row per message the
//! `delivery.send` job decided on, with the gates it passed, and the address it was sent to read
//! at send time.
//!
//! `messages` is person-scoped. Erasing a person deletes their rows (`privacy::delete`), and on a
//! merge they stay on the id they were written for (see `rewrite_merged_person`); a read of the log
//! goes through `person_merges` ([`list_person_messages`]). A row holds no address.

mod address;
mod log;

pub use address::read_send_address;
pub use log::{
    GateRecord, Message, MessageStatus, NewMessage, find_message, insert_message,
    list_person_messages, mark_message_sending, message_status,
};
