//! The send limits of a workspace: frequency caps and quiet hours (gates 5 and 6 of the send
//! pipeline, architecture 10.1).
//!
//! `settings` holds the two settings and the one write that replaces both. `slots` is the cap
//! check: it counts the person's messages that hold a slot and reserves one for the message in a
//! short transaction of its own, under a per-person advisory lock.
//!
//! Neither setting is person-scoped. The cap counts read `messages`, which erasure already covers.

mod settings;
mod slots;

pub use settings::{
    CapMode, FrequencyCap, NewFrequencyCap, QuietHours, SendLimits, read_send_limits,
    replace_send_limits,
};
pub use slots::{CAP_LOCK_CLASS, CapVerdict, check_frequency_caps, record_message_deferral};
