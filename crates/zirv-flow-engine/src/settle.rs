//! What the flow's settings do to a running enrollment (architecture 9.4): the goal and the exit
//! criteria. The caller does the matching (an event against the goal or a criterion, the
//! evaluator being its own); this decides the outcome, so the runtime and test mode agree.

use zirv_flow_core::flow::{ExitCriterion, FlowSettings};

use crate::hash::settings_goal_key;
use crate::state::{
    Effect, EffectKind, EnrollmentState, ExitReason, Outcome, Status, Transition, WaitChange,
};

/// What the caller found true of one input, which it matched against the flow's settings.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Matched {
    /// The input reaches the flow's goal.
    pub goal: bool,
    /// The input is one of the event exit criteria.
    pub exit_event: bool,
}

/// The transition the settings cause, or `None` when they do nothing: nothing matched, the
/// settings have no such goal or criterion, or the enrollment is not active or waiting (a failed
/// one waits for an operator, an awaiting one is inside a step, an ended one is over).
///
/// A reached goal records a conversion (an internal [`EffectKind::GoalConversion`] whose step
/// key is [`settings_goal_key`], one per enrollment: the caller records the conversion once, and
/// a goal reached again carries the same key) and, with `exit_on_goal` or a `goal_reached` criterion, ends the enrollment as
/// exited. A matched event criterion ends it too. A goal alone leaves the state as it is
/// ([`Outcome::Converted`]). An exit grows `state_version`, so any timer or event for the older
/// state is stale, and clears an event wait.
pub fn settle(
    settings: &FlowSettings,
    state: &EnrollmentState,
    matched: Matched,
) -> Option<Transition> {
    if !matches!(state.status, Status::Active | Status::Waiting) {
        return None;
    }
    let goal = matched.goal && settings.goal.is_some();
    let has = |wanted: fn(&ExitCriterion) -> bool| settings.exit_criteria.iter().any(wanted);
    let reason = if goal && settings.exit_on_goal {
        Some(ExitReason::Goal)
    } else if goal && has(|c| matches!(c, ExitCriterion::GoalReached {})) {
        Some(ExitReason::GoalCriterion)
    } else if matched.exit_event && has(|c| matches!(c, ExitCriterion::Event(_))) {
        Some(ExitReason::ExitEvent)
    } else {
        None
    };
    if !goal && reason.is_none() {
        return None;
    }
    let effects = if goal {
        vec![Effect {
            node: state.node.clone(),
            step_key: settings_goal_key(state.enrollment_id),
            effect_key: None,
            kind: EffectKind::GoalConversion,
        }]
    } else {
        Vec::new()
    };
    let Some(reason) = reason else {
        return Some(Transition {
            state: state.clone(),
            visited: Vec::new(),
            effects,
            wake: None,
            waits: Vec::new(),
            outcome: Outcome::Converted,
            zone_fallback: None,
        });
    };
    let waits = state
        .awaiting
        .iter()
        .map(|name| WaitChange::Clear {
            event_name: name.clone(),
        })
        .collect();
    Some(Transition {
        state: EnrollmentState {
            enrollment_id: state.enrollment_id,
            node: state.node.clone(),
            state_version: state.state_version.checked_add(1)?,
            status: Status::Exited,
            wake_at: None,
            awaiting: None,
        },
        visited: Vec::new(),
        effects,
        wake: None,
        waits,
        outcome: Outcome::Exited { reason },
        zone_fallback: None,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;
    use zirv_flow_core::flow::{EventMatch, Goal, Reentry, Trigger};

    use super::*;

    fn event(name: &str) -> EventMatch {
        EventMatch {
            name: name.into(),
            filters: vec![json!({"prop": "total", "op": "gt", "value": 1})],
        }
    }

    fn settings(goal: bool, exit_on_goal: bool, criteria: Vec<ExitCriterion>) -> FlowSettings {
        FlowSettings {
            trigger: Trigger::Manual {},
            reentry: Reentry::Once {},
            goal: goal.then(|| Goal::Event(event("bought"))),
            exit_on_goal,
            exit_criteria: criteria,
        }
    }

    fn state(status: Status, awaiting: Option<&str>) -> EnrollmentState {
        EnrollmentState {
            enrollment_id: Uuid::from_u128(1),
            node: "n".into(),
            state_version: 4,
            status,
            wake_at: None,
            awaiting: awaiting.map(str::to_owned),
        }
    }

    const GOAL: Matched = Matched {
        goal: true,
        exit_event: false,
    };
    const EXIT: Matched = Matched {
        goal: false,
        exit_event: true,
    };

    #[test]
    fn a_goal_without_exit_records_a_conversion_and_leaves_the_state() {
        let waiting = state(Status::Waiting, Some("x"));
        let moved = settle(&settings(true, false, vec![]), &waiting, GOAL).unwrap();

        assert_eq!(moved.outcome, Outcome::Converted);
        assert_eq!(moved.state, waiting);
        assert_eq!(moved.effects.len(), 1);
        assert_eq!(moved.effects[0].kind, EffectKind::GoalConversion);
        assert_eq!(
            moved.effects[0].step_key,
            format!("{}:settings.goal", Uuid::from_u128(1))
        );
        assert!(!moved.effects[0].is_external() && moved.waits.is_empty());
    }

    #[test]
    fn exit_on_goal_ends_the_enrollment_as_exited_and_clears_the_wait() {
        let waiting = state(Status::Waiting, Some("x"));
        let moved = settle(&settings(true, true, vec![]), &waiting, GOAL).unwrap();

        assert_eq!(
            moved.outcome,
            Outcome::Exited {
                reason: ExitReason::Goal
            }
        );
        assert_eq!(moved.state.status, Status::Exited);
        assert_eq!(moved.state.state_version, 5);
        assert!(moved.state.awaiting.is_none() && moved.state.wake_at.is_none());
        assert_eq!(
            moved.waits,
            [WaitChange::Clear {
                event_name: "x".into()
            }]
        );
        assert_eq!(moved.effects[0].kind, EffectKind::GoalConversion);
    }

    #[test]
    fn a_goal_reached_criterion_follows_the_goal_and_an_event_criterion_ends_it_alone() {
        let active = state(Status::Active, None);
        let by_goal = settings(true, false, vec![ExitCriterion::GoalReached {}]);
        let moved = settle(&by_goal, &active, GOAL).unwrap();
        assert_eq!(
            moved.outcome,
            Outcome::Exited {
                reason: ExitReason::GoalCriterion
            }
        );
        assert_eq!(moved.effects.len(), 1);

        let by_event = settings(false, false, vec![ExitCriterion::Event(event("left"))]);
        let moved = settle(&by_event, &active, EXIT).unwrap();
        assert_eq!(
            moved.outcome,
            Outcome::Exited {
                reason: ExitReason::ExitEvent
            }
        );
        assert!(moved.effects.is_empty());
    }

    #[test]
    fn only_an_active_or_waiting_enrollment_is_settled() {
        let full = settings(true, true, vec![ExitCriterion::Event(event("left"))]);
        for status in [Status::Active, Status::Waiting] {
            assert!(settle(&full, &state(status, None), GOAL).is_some());
            assert!(settle(&full, &state(status, None), EXIT).is_some());
        }
        for status in [Status::Failed, Status::AwaitingConditions] {
            assert!(settle(&full, &state(status, None), GOAL).is_none());
            assert!(settle(&full, &state(status, None), EXIT).is_none());
        }
    }

    #[test]
    fn the_conversion_key_is_one_per_enrollment_whatever_the_node() {
        let plain = settings(true, false, vec![]);
        let mut elsewhere = state(Status::Waiting, None);
        elsewhere.node = "other".into();

        let first = settle(&plain, &state(Status::Active, None), GOAL).unwrap();
        let again = settle(&plain, &elsewhere, GOAL).unwrap();

        assert_eq!(first.effects[0].step_key, again.effects[0].step_key);
        assert_eq!(
            first.effects[0].step_key,
            settings_goal_key(Uuid::from_u128(1))
        );
        assert_ne!(
            first.effects[0].step_key,
            settings_goal_key(Uuid::from_u128(2))
        );
    }
    #[test]
    fn nothing_happens_without_a_match_a_setting_or_a_live_enrollment() {
        let active = state(Status::Active, None);
        let full = settings(true, true, vec![ExitCriterion::Event(event("left"))]);
        assert!(settle(&full, &active, Matched::default()).is_none());
        // No goal in the settings: a goal match means nothing. No criterion: neither does an exit.
        assert!(settle(&settings(false, true, vec![]), &active, GOAL).is_none());
        assert!(settle(&settings(true, true, vec![]), &active, EXIT).is_none());
        for ended in [Status::Completed, Status::Exited] {
            assert!(settle(&full, &state(ended, None), GOAL).is_none());
            assert!(settle(&full, &state(ended, None), EXIT).is_none());
        }
    }
}
