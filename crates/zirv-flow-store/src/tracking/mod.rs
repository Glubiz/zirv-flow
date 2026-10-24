//! What open, click and unsubscribe tracking reads and writes (architecture 10.2): the links of a
//! sent message, the engagement events, and the consent change an unsubscribe link makes.
//!
//! Nothing here holds an address, an IP or a user agent. An event carries ids and the machine
//! verdict; a decision carries ids and the source code.

mod engagement;
mod links;
mod unsubscribe;

pub use engagement::{Engagement, record_engagement};
pub use links::{TrackedMessage, find_message_link, find_tracked_message, record_message_links};
pub use unsubscribe::{Unsubscribed, unsubscribe_message};
