//! The primary keys of a batch of events, for a statement that must probe them by key.
//!
//! The batch statements of retention and of the merge rewrite used to join the events to the
//! keys they had just selected (`WHERE e.occurred_at = d.occurred_at AND e.id = d.id`). The
//! planner estimated the keys at thousands and hash-joined them against a seq scan of every
//! populated partition on each batch (1.7M events in 26 partitions: about 350 ms and 50,000
//! buffers to delete 5,000 rows, flat per batch). The keys are read first and bound as arrays
//! with the range they span, so the statement names its partitions with constants and probes the
//! primary key `(occurred_at, id)` for each.

use time::OffsetDateTime;
use uuid::Uuid;

/// The keys of some events, as the two arrays an `unnest` joins back.
pub(crate) struct EventKeys {
    pub(crate) occurred_at: Vec<OffsetDateTime>,
    pub(crate) ids: Vec<Uuid>,
}

impl EventKeys {
    pub(crate) fn new(keys: Vec<(OffsetDateTime, Uuid)>) -> Self {
        let (occurred_at, ids) = keys.into_iter().unzip();
        Self { occurred_at, ids }
    }

    /// The oldest and the newest `occurred_at` of the keys; none when there are none.
    pub(crate) fn span(&self) -> Option<(OffsetDateTime, OffsetDateTime)> {
        Some((
            *self.occurred_at.iter().min()?,
            *self.occurred_at.iter().max()?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    #[test]
    fn keys_split_into_two_arrays_and_report_the_range_they_span() {
        let early = datetime!(2026-01-01 00:00 UTC);
        let late = datetime!(2026-03-01 00:00 UTC);
        let keys = EventKeys::new(vec![
            (late, Uuid::from_u128(1)),
            (early, Uuid::from_u128(2)),
        ]);

        assert_eq!(keys.occurred_at, [late, early]);
        assert_eq!(keys.ids, [Uuid::from_u128(1), Uuid::from_u128(2)]);
        assert_eq!(keys.span(), Some((early, late)));
        assert!(EventKeys::new(Vec::new()).span().is_none());
    }
}
