//! Event forwarding (architecture 12.5 and 14): rules that select accepted events for a
//! destination, and a transactional outbox the selected events are copied into by the same
//! transaction that writes them.
//!
//! Every path that writes an event calls [`enqueue_forwarding`] after its inserts, in its transaction. A
//! source scan test fails when a file that inserts events is not on the list of known writers,
//! so a new writer cannot skip the outbox silently. Replay writes events no client ever sent
//! twice and adds no outbox row.
//!
//! `forwarding_outbox` is person-scoped: erasure deletes a person's rows
//! (`privacy::delete_person`) and a merge re-points them.

mod outbox;
mod rules;
mod sql;

#[cfg(test)]
pub(crate) use rules::tests as support;

pub(crate) use outbox::enqueue_forwarding_of;
pub use outbox::{
    ClaimedRow, DELIVERED_RETENTION, DeadLetter, FORWARD_JOB_KIND, ForwardRef, ForwardingStat,
    QueueStatus, Redrive, Settled, claim_due, defer_row, enqueue_forwarding,
    enqueue_segment_forwarding, ensure_forward_job, forwarding_report, forwarding_status,
    list_dead_letters, next_due, purge_delivered, redrive, settle_row,
};
pub use rules::{
    ForwardingRule, NewRule, RuleUpdate, RuleWrite, create_rule, delete_rule, find_rule,
    list_rules, update_rule,
};
