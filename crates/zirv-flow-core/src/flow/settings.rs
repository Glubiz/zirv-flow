//! The flow settings that sit beside the graph (architecture sections 9.1 and 9.4): the
//! trigger, re-entry, the optional goal and the exit criteria.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::graph::EventMatch;

/// The `type` of every trigger.
pub const TRIGGER_TYPES: [&str; 5] = ["segment", "event", "schedule", "manual", "api"];

/// How a person comes into the flow.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Trigger {
    /// Entering or leaving a segment. `enroll_current` also enrolls the members at publish.
    Segment {
        segment_id: Uuid,
        on: SegmentChange,
        #[serde(default)]
        enroll_current: bool,
    },
    /// An event with property filters.
    Event(EventMatch),
    /// A cron slot, in the trigger's own time zone, for a segment or everyone.
    Schedule {
        cron: String,
        timezone: String,
        audience: Audience,
    },
    /// An operator enrolls people by hand.
    Manual,
    /// `POST .../flows/{id}/enrollments`.
    Api,
}

impl Trigger {
    /// The `type` on the wire; one of [`TRIGGER_TYPES`].
    pub const fn type_name(&self) -> &'static str {
        match self {
            Self::Segment { .. } => "segment",
            Self::Event(_) => "event",
            Self::Schedule { .. } => "schedule",
            Self::Manual => "manual",
            Self::Api => "api",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentChange {
    Entered,
    Exited,
}

/// Who a schedule trigger enrolls.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Audience {
    Everyone,
    Segment { segment_id: Uuid },
}

/// Whether a person may enter again.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Reentry {
    /// Never again after the first entry.
    Once,
    /// Again once the active enrollment has ended.
    OnceAtATime,
    /// Again, no sooner than `min_interval_seconds` after the last entry (0 for no wait).
    Always {
        #[serde(default)]
        min_interval_seconds: u64,
    },
}

/// A conversion: an event with the filters of an event trigger, or entry into a segment.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Goal {
    Event(EventMatch),
    Segment { segment_id: Uuid },
}

/// A reason to end an active enrollment.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExitCriterion {
    /// The person leaves the trigger segment (a segment trigger only).
    LeavesSegment,
    /// The event occurs.
    Event(EventMatch),
    /// The flow's goal is reached (needs a goal).
    GoalReached,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FlowSettings {
    pub trigger: Trigger,
    pub reentry: Reentry,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<Goal>,
    /// End the enrollment when the goal is reached.
    #[serde(default)]
    pub exit_on_goal: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exit_criteria: Vec<ExitCriterion>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event() -> EventMatch {
        EventMatch {
            name: "signed_up".into(),
            filters: vec![json!({"prop": "plan", "op": "eq", "value": "pro"})],
        }
    }

    #[test]
    fn every_trigger_round_trips_through_json() {
        let triggers = [
            Trigger::Segment {
                segment_id: Uuid::nil(),
                on: SegmentChange::Entered,
                enroll_current: true,
            },
            Trigger::Segment {
                segment_id: Uuid::nil(),
                on: SegmentChange::Exited,
                enroll_current: false,
            },
            Trigger::Event(event()),
            Trigger::Schedule {
                cron: "0 9 * * 1".into(),
                timezone: "Europe/Copenhagen".into(),
                audience: Audience::Everyone,
            },
            Trigger::Schedule {
                cron: "0 9 * * 1".into(),
                timezone: "UTC".into(),
                audience: Audience::Segment {
                    segment_id: Uuid::nil(),
                },
            },
            Trigger::Manual,
            Trigger::Api,
        ];
        let names: Vec<&str> = triggers.iter().map(Trigger::type_name).collect();
        for name in TRIGGER_TYPES {
            assert!(names.contains(&name), "{name} has no example");
        }
        for trigger in triggers {
            let wire = serde_json::to_value(&trigger).unwrap();
            assert_eq!(wire["type"], trigger.type_name());
            assert_eq!(serde_json::from_value::<Trigger>(wire).unwrap(), trigger);
        }
    }

    #[test]
    fn every_setting_round_trips_and_optional_ones_may_be_omitted() {
        let full = FlowSettings {
            trigger: Trigger::Api,
            reentry: Reentry::Always {
                min_interval_seconds: 86_400,
            },
            goal: Some(Goal::Event(event())),
            exit_on_goal: true,
            exit_criteria: vec![
                ExitCriterion::LeavesSegment,
                ExitCriterion::Event(event()),
                ExitCriterion::GoalReached,
            ],
        };
        let wire = serde_json::to_value(&full).unwrap();
        assert_eq!(serde_json::from_value::<FlowSettings>(wire).unwrap(), full);

        let segment_goal = FlowSettings {
            goal: Some(Goal::Segment {
                segment_id: Uuid::nil(),
            }),
            ..full
        };
        let wire = serde_json::to_value(&segment_goal).unwrap();
        assert_eq!(
            serde_json::from_value::<FlowSettings>(wire).unwrap(),
            segment_goal
        );

        let minimal: FlowSettings = serde_json::from_value(
            json!({"trigger": {"type": "manual"}, "reentry": {"mode": "once"}}),
        )
        .unwrap();
        assert_eq!(minimal.reentry, Reentry::Once);
        assert!(
            minimal.goal.is_none() && !minimal.exit_on_goal && minimal.exit_criteria.is_empty()
        );
        for reentry in [
            Reentry::OnceAtATime,
            Reentry::Always {
                min_interval_seconds: 0,
            },
        ] {
            let wire = serde_json::to_value(reentry).unwrap();
            assert_eq!(serde_json::from_value::<Reentry>(wire).unwrap(), reentry);
        }
    }
}
