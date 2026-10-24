//! Destinations (architecture 5.3, 10.1 and 14): the configured external systems messages are
//! delivered to, the cross-worker rate bucket (ruling OQ-9) and the record of each delivery
//! attempt. A destination's credential is a sealed secret, referenced by id and never stored
//! here.
//!
//! None of these tables is person-scoped, so the erasure scan has nothing to look for in them:
//! `destinations` and the buckets describe the system, and `delivery_attempts` names the message,
//! forwarding or sync item an attempt was for, never a person id and never an address.

mod attempts;
mod bucket;
mod crud;

pub use attempts::{
    AttemptOutcome, DeliveryAttempt, DestinationStat, NewDeliveryAttempt, StatusClass, SubjectKind,
    count_delivery_attempt, count_delivery_attempt_record_failure, destination_report,
    list_delivery_attempts, record_delivery_attempt,
};
pub use bucket::{
    Admission, BucketState, DEFAULT_RETRY_AFTER, LEASE, MAX_BLOCK, MAX_RETRY_AFTER, RateGate,
    bucket_state, throttle_bucket,
};
pub use crud::{
    Destination, DestinationUpdate, NewDestination, archive_destination, create_destination,
    find_destination, find_destination_for_update, find_destination_workspace, list_destinations,
    update_destination,
};
