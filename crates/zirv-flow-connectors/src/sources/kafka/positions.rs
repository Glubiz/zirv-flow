//! The checkpoint of a Kafka read: where each partition resumes and how far its log reaches.
//! The engine stores and returns it without looking inside (architecture 7.2); the consumer job
//! parses it with this type to write the offsets.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::super::types::Checkpoint;

/// One partition: the next offset to read, and the high watermark seen at the last fetch.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PartitionPosition {
    pub next: i64,
    /// The high watermark; absent in a start checkpoint built from offsets stored before a fetch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<i64>,
}

/// The positions of the partitions of one topic, keyed by partition number as text.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Positions {
    #[serde(default)]
    pub partitions: BTreeMap<String, PartitionPosition>,
}

impl Positions {
    pub fn get(&self, partition: i32) -> Option<PartitionPosition> {
        self.partitions.get(&partition.to_string()).copied()
    }

    pub fn set(&mut self, partition: i32, position: PartitionPosition) {
        self.partitions.insert(partition.to_string(), position);
    }

    /// The positions as a batch checkpoint.
    pub fn to_checkpoint(&self) -> Checkpoint {
        Checkpoint(serde_json::to_value(self).unwrap_or_default())
    }

    /// Reads the positions out of a checkpoint a read wrote, or one the job built. `None` when it
    /// is not a Kafka checkpoint.
    pub fn from_checkpoint(checkpoint: &Checkpoint) -> Option<Self> {
        serde_json::from_value(checkpoint.0.clone()).ok()
    }
}

/// How long a read lasts and when it ends. Rides in the start checkpoint, so the connector needs
/// no second API: a read without a plan ends as soon as every partition is caught up.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadPlan {
    /// The most time the read may run; `None` is the default budget of a bounded read.
    pub budget: Option<Duration>,
    /// End when every partition is caught up (preview, discovery) instead of waiting for more.
    pub stop_when_caught_up: bool,
}

/// The budget of a read that has none: a bounded read must still end.
pub(super) const DEFAULT_BUDGET: Duration = Duration::from_secs(30);

impl Default for ReadPlan {
    fn default() -> Self {
        Self {
            budget: None,
            stop_when_caught_up: true,
        }
    }
}

#[derive(Deserialize, Serialize)]
struct PlanFields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    budget_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stop_when_caught_up: Option<bool>,
}

impl ReadPlan {
    /// The start checkpoint: `positions` with this plan.
    pub fn into_checkpoint(self, positions: &Positions) -> Checkpoint {
        let mut value = serde_json::to_value(positions).unwrap_or_default();
        let plan = PlanFields {
            budget_ms: self
                .budget
                .map(|budget| u64::try_from(budget.as_millis()).unwrap_or(u64::MAX)),
            stop_when_caught_up: Some(self.stop_when_caught_up),
        };
        if let (Some(object), Ok(serde_json::Value::Object(fields))) =
            (value.as_object_mut(), serde_json::to_value(plan))
        {
            object.extend(fields);
        }
        Checkpoint(value)
    }

    /// The plan and the positions of a start checkpoint; the defaults for `None` or a checkpoint
    /// without a plan.
    pub(super) fn from_checkpoint(checkpoint: Option<&Checkpoint>) -> (Self, Positions) {
        let Some(checkpoint) = checkpoint else {
            return (Self::default(), Positions::default());
        };
        let positions = Positions::from_checkpoint(checkpoint).unwrap_or_default();
        let fields: Option<PlanFields> = serde_json::from_value(checkpoint.0.clone()).ok();
        let plan = Self {
            budget: fields
                .as_ref()
                .and_then(|fields| fields.budget_ms)
                .map(Duration::from_millis),
            stop_when_caught_up: fields
                .and_then(|fields| fields.stop_when_caught_up)
                .unwrap_or(true),
        };
        (plan, positions)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn positions_round_trip_through_a_checkpoint_in_the_documented_shape() {
        let mut positions = Positions::default();
        positions.set(
            0,
            PartitionPosition {
                next: 12,
                end: Some(40),
            },
        );

        let checkpoint = positions.to_checkpoint();

        assert_eq!(
            checkpoint.0,
            json!({"partitions": {"0": {"next": 12, "end": 40}}})
        );
        assert_eq!(Positions::from_checkpoint(&checkpoint), Some(positions));
    }

    #[test]
    fn a_stored_offset_without_a_watermark_is_a_valid_start() {
        let checkpoint = Checkpoint(json!({"partitions": {"3": {"next": 7}}}));

        let positions = Positions::from_checkpoint(&checkpoint).unwrap();

        assert_eq!(
            positions.get(3),
            Some(PartitionPosition { next: 7, end: None })
        );
        assert_eq!(positions.get(4), None);
    }

    #[test]
    fn something_that_is_not_positions_is_refused() {
        assert_eq!(Positions::from_checkpoint(&Checkpoint(json!("x"))), None);
        assert_eq!(
            Positions::from_checkpoint(&Checkpoint(json!({"partitions": {"0": {"next": "a"}}}))),
            None
        );
    }

    #[test]
    fn a_plan_rides_in_the_start_checkpoint_beside_the_positions() {
        let mut positions = Positions::default();
        positions.set(1, PartitionPosition { next: 5, end: None });
        let plan = ReadPlan {
            budget: Some(Duration::from_millis(45_000)),
            stop_when_caught_up: false,
        };

        let checkpoint = plan.into_checkpoint(&positions);
        let (read, parsed) = ReadPlan::from_checkpoint(Some(&checkpoint));

        assert_eq!(checkpoint.0["budget_ms"], 45_000);
        assert_eq!(checkpoint.0["stop_when_caught_up"], false);
        assert_eq!(read, plan);
        assert_eq!(parsed, positions);
    }

    #[test]
    fn no_checkpoint_means_a_bounded_read_from_the_start() {
        let (plan, positions) = ReadPlan::from_checkpoint(None);

        assert!(plan.stop_when_caught_up);
        assert_eq!(plan.budget, None);
        assert!(positions.partitions.is_empty());
    }
}
