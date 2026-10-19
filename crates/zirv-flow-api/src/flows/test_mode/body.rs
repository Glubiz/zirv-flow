//! The wire forms of test mode: the request, the snapshot a walk reads and returns, and the
//! response (architecture 9.4). The snapshot is the evaluator's, in JSON, so a response can be
//! re-posted as the next request's person.

use std::collections::BTreeSet;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::consent::{Channel, ConsentStatus};
use zirv_flow_segments::{PersonSnapshot, SnapshotConsent, SnapshotEvent};

use crate::flows::schema::GRAPH;
use crate::problem::ApiError;

fn graph_schema() -> utoipa::openapi::RefOr<utoipa::openapi::Schema> {
    utoipa::openapi::RefOr::Ref(utoipa::openapi::Ref::from_schema_name(GRAPH))
}

/// The request of `POST .../flows/{id}/test`.
///
/// The graph is the flow's draft unless `version` or `graph` is given (not both). The person is
/// `person_id` or `snapshot` (exactly one).
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TestRequest {
    /// Walk this published version instead of the draft.
    pub(crate) version: Option<i32>,
    /// Walk this graph instead of the saved draft. It is checked as a publish checks structure.
    #[schema(schema_with = graph_schema)]
    pub(crate) graph: Option<Value>,
    /// A real person, whose bounded snapshot is read (read only) and returned. Needs the right
    /// to see personal values (workspace admin, owner or an `admin` key), because the snapshot
    /// holds them.
    pub(crate) person_id: Option<Uuid>,
    /// A synthetic person, or the snapshot a previous call returned.
    pub(crate) snapshot: Option<SnapshotBody>,
    /// The virtual time the walk starts at; the database's time when omitted. Microsecond
    /// precision.
    #[serde(default, with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub(crate) start_at: Option<OffsetDateTime>,
    /// The enrollment id the walk runs under. It fixes `split.random` and the effect keys; the
    /// default is derived from the flow and the person, so a call is repeatable.
    pub(crate) enrollment_id: Option<Uuid>,
    /// Steps applied in order after the first run; at most 50, times never going back.
    #[serde(default)]
    pub(crate) script: Vec<ScriptStepBody>,
}

/// One step of the script: exactly one of `advance_to` and `inject`.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScriptStepBody {
    /// Move the virtual clock to this time, firing every timer due by then.
    #[serde(default, with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub(crate) advance_to: Option<OffsetDateTime>,
    /// Deliver an event at a time.
    pub(crate) inject: Option<InjectBody>,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct InjectBody {
    pub(crate) name: String,
    #[serde(default)]
    #[schema(value_type = Object)]
    pub(crate) properties: Map<String, Value>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub(crate) at: OffsetDateTime,
}

/// What the reference evaluator reads about one person. `capped_events` and
/// `events_complete_after` carry the caps of a real load, and a synthetic snapshot can set them
/// to see a walk stop with `unable_to_evaluate`.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct SnapshotBody {
    /// The person the snapshot was read from. A real-person walk fills it in, so the webhook
    /// envelope of a replay names the same person.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) person_id: Option<Uuid>,
    /// Stored trait values by key.
    #[schema(value_type = Object)]
    pub(crate) traits: Map<String, Value>,
    pub(crate) events: Vec<SnapshotEventBody>,
    /// Event names whose events were cut off when the snapshot was loaded.
    pub(crate) capped_events: Vec<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub(crate) events_complete_after: Option<OffsetDateTime>,
    pub(crate) lists: Vec<Uuid>,
    pub(crate) consent: Vec<SnapshotConsentBody>,
    /// `email`, `phone`, `anonymous_id` or `external:<namespace>`; never values.
    pub(crate) identifier_kinds: Vec<String>,
    /// The segments the person is an open member of.
    pub(crate) segments: Vec<Uuid>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SnapshotEventBody {
    pub(crate) name: String,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub(crate) occurred_at: OffsetDateTime,
    #[serde(default)]
    #[schema(value_type = Object)]
    pub(crate) properties: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SnapshotConsentBody {
    /// `email`.
    pub(crate) channel: String,
    /// The topic id; null is the channel itself.
    pub(crate) topic: Option<Uuid>,
    /// `subscribed`, `unsubscribed`, `pending_confirmation` or `unknown`.
    pub(crate) status: String,
}

impl SnapshotBody {
    /// The wire form of an evaluator snapshot, sets in sorted order so a response is repeatable.
    pub(crate) fn of(snapshot: &PersonSnapshot) -> Self {
        Self {
            person_id: None,
            traits: snapshot.traits.clone(),
            events: snapshot
                .events
                .iter()
                .map(|event| SnapshotEventBody {
                    name: event.name.clone(),
                    occurred_at: event.occurred_at,
                    properties: event.properties.clone(),
                })
                .collect(),
            capped_events: snapshot.capped_events.iter().cloned().collect(),
            events_complete_after: snapshot.events_complete_after,
            lists: snapshot.lists.iter().copied().collect(),
            consent: snapshot
                .consent
                .iter()
                .map(|row| SnapshotConsentBody {
                    channel: row.channel.as_str().to_owned(),
                    topic: row.topic,
                    status: row.status.as_str().to_owned(),
                })
                .collect(),
            identifier_kinds: snapshot.identifier_kinds.iter().cloned().collect(),
            segments: snapshot.segments.iter().copied().collect(),
        }
    }

    /// The evaluator's snapshot, or the 422 for a channel or status it does not know.
    pub(crate) fn to_snapshot(&self) -> Result<PersonSnapshot, ApiError> {
        let consent = self
            .consent
            .iter()
            .map(|row| {
                Ok(SnapshotConsent {
                    channel: Channel::from_str(&row.channel).map_err(|_| {
                        ApiError::unprocessable("snapshot.consent: unknown channel")
                    })?,
                    topic: row.topic,
                    status: ConsentStatus::from_str(&row.status)
                        .map_err(|_| ApiError::unprocessable("snapshot.consent: unknown status"))?,
                })
            })
            .collect::<Result<Vec<_>, ApiError>>()?;
        Ok(PersonSnapshot {
            traits: self.traits.clone(),
            events: self
                .events
                .iter()
                .map(|event| SnapshotEvent {
                    name: event.name.clone(),
                    occurred_at: event.occurred_at,
                    properties: event.properties.clone(),
                })
                .collect(),
            capped_events: self.capped_events.iter().cloned().collect(),
            events_complete_after: self.events_complete_after,
            lists: self.lists.iter().copied().collect::<BTreeSet<_>>(),
            consent,
            identifier_kinds: self.identifier_kinds.iter().cloned().collect(),
            segments: self.segments.iter().copied().collect(),
        })
    }
}

/// What a leaf condition saw, flattened out of the evaluation tree.
#[derive(Clone, Debug, PartialEq, Serialize, ToSchema)]
pub(crate) struct ObservedBody {
    /// The JSON pointer of the leaf in the condition.
    pub(crate) pointer: String,
    /// `pass`, `fail` or `unknown` (a capped event name).
    pub(crate) outcome: &'static str,
    /// `{"value": ..}` for a trait, `{"missing": true}`, `{"count": n}` or `{"value": .., "count": n}`
    /// for events, `{"present": bool}` for a list, segment, consent or identifier.
    #[schema(value_type = Object)]
    pub(crate) observed: Value,
}

/// How one condition of a branch came out.
#[derive(Clone, Debug, PartialEq, Serialize, ToSchema)]
pub(crate) struct ConditionBody {
    pub(crate) index: usize,
    /// `pass`, `fail`, `unknown`, or `not_evaluated` for a condition after the first that passed.
    pub(crate) outcome: &'static str,
    pub(crate) observed: Vec<ObservedBody>,
}

/// A node the walk ran.
#[derive(Clone, Debug, PartialEq, Serialize, ToSchema)]
pub(crate) struct PathStepBody {
    pub(crate) node: String,
    /// The node type, such as `wait.duration`.
    #[serde(rename = "type")]
    pub(crate) node_type: &'static str,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub(crate) at: OffsetDateTime,
    /// `passed`, `waiting` (asleep at the end of the walk), `branched`, `completed`, `exited`,
    /// `failed`, `unable_to_evaluate`, or for a step of type `settings.goal` / `settings.exit`
    /// (the flow settings, not a node) `converted` / `exited`.
    pub(crate) outcome: &'static str,
    /// The output port the walk left through; null when it stopped here or the port is not
    /// unambiguous from the edges.
    pub(crate) port: Option<String>,
    /// The branch's conditions, in order.
    pub(crate) branch: Option<Vec<ConditionBody>>,
    /// When a wait node woke.
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub(crate) woke_at: Option<OffsetDateTime>,
    /// `timer`, `event` or `timeout`.
    pub(crate) woke_by: Option<&'static str>,
}

/// A captured effect. Nothing is executed.
#[derive(Clone, Debug, PartialEq, Serialize, ToSchema)]
pub(crate) struct EffectBody {
    pub(crate) node: String,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub(crate) at: OffsetDateTime,
    /// The step idempotency key, `{enrollment_id}:{node_id}`.
    pub(crate) step_key: String,
    /// The key an external effect carries to its destination; null for internal ones.
    pub(crate) effect_key: Option<String>,
    /// `update_trait`, `list`, `goal` or `webhook`.
    pub(crate) kind: &'static str,
    /// What the node asked for. A webhook shows the `url` and the `envelope` that would be sent
    /// (never the signing secret or a signature).
    #[schema(value_type = Object)]
    pub(crate) detail: Value,
}

/// A wait the walk went into.
#[derive(Clone, Debug, PartialEq, Serialize, ToSchema)]
pub(crate) struct WaitBody {
    pub(crate) node: String,
    #[serde(rename = "type")]
    pub(crate) node_type: &'static str,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub(crate) started_at: OffsetDateTime,
    /// The time the timer fires (the timeout, for `wait.until_event`).
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub(crate) wake_at: OffsetDateTime,
    /// The event name a `wait.until_event` waits for.
    pub(crate) awaiting: Option<String>,
    /// `open`, `timer`, `event`, `timeout`, or `exit` (the flow settings ended the enrollment).
    pub(crate) resolved: &'static str,
}

/// A timer the engine asked for.
#[derive(Clone, Debug, PartialEq, Serialize, ToSchema)]
pub(crate) struct WakeBody {
    pub(crate) node: String,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub(crate) at: OffsetDateTime,
    /// The unique key of the `flow.step` job the runtime would enqueue.
    pub(crate) key: String,
    /// Whether the walk reached `at` and fired it.
    pub(crate) fired: bool,
}

/// What a script step did.
#[derive(Clone, Debug, PartialEq, Serialize, ToSchema)]
pub(crate) struct ScriptResultBody {
    pub(crate) index: usize,
    /// `advance_to` or `inject`.
    pub(crate) kind: &'static str,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub(crate) at: OffsetDateTime,
    /// `advanced`, `event_delivered` (it satisfied the wait), `event_recorded` (it did not match
    /// a wait), `event_ignored` (the filters did not match), `goal_reached` (the flow goal, recorded as a
    /// conversion; the enrollment goes on), `goal_exit` or `exit_criterion` (the settings ended the
    /// enrollment as exited) or `skipped` (the walk had stopped).
    pub(crate) outcome: &'static str,
}

/// Where the enrollment ended up.
#[derive(Clone, Debug, PartialEq, Serialize, ToSchema)]
pub(crate) struct FinalStateBody {
    /// `active`, `awaiting_conditions`, `waiting`, `completed`, `exited` or `failed`.
    pub(crate) status: String,
    /// The node the enrollment is at or stopped at.
    pub(crate) node: String,
    pub(crate) state_version: u64,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub(crate) wake_at: Option<OffsetDateTime>,
    pub(crate) awaiting: Option<String>,
    /// The virtual clock when the walk ended.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub(crate) clock: OffsetDateTime,
}

/// Why the walk stopped before the graph or the script ended.
#[derive(Clone, Debug, PartialEq, Serialize, ToSchema)]
pub(crate) struct HaltBody {
    /// `unable_to_evaluate` (a condition reads an event name the snapshot capped),
    /// `invalid_condition` (a condition or filter that does not validate against the
    /// workspace), `step_failed` (an effect could not apply) or `engine_error`.
    pub(crate) reason: &'static str,
    pub(crate) node: Option<String>,
    /// The index of the condition in the branch.
    pub(crate) condition: Option<usize>,
    /// The JSON pointer inside the condition.
    pub(crate) pointer: Option<String>,
    /// The capped event names the condition reads.
    pub(crate) capped_events: Vec<String>,
    /// The most events one load keeps (`unable_to_evaluate` only).
    pub(crate) cap: Option<usize>,
    pub(crate) message: String,
}

/// The person's traits and lists after the walk's internal effects.
#[derive(Clone, Debug, PartialEq, Serialize, ToSchema)]
pub(crate) struct PersonAfterBody {
    #[schema(value_type = Object)]
    pub(crate) traits: Map<String, Value>,
    pub(crate) lists: Vec<Uuid>,
}

/// The result of a walk.
#[derive(Debug, PartialEq, Serialize, ToSchema)]
pub(crate) struct TestBody {
    pub(crate) flow_id: Uuid,
    /// `draft`, `version` or `inline`.
    pub(crate) source: &'static str,
    /// The version walked; null for the draft and an inline graph.
    pub(crate) version: Option<i32>,
    /// The person walked: the survivor when `person_id` was merged into another, or the person a
    /// posted snapshot names; null for a synthetic one.
    pub(crate) person_id: Option<Uuid>,
    pub(crate) merged_from: Option<Uuid>,
    pub(crate) enrollment_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub(crate) start_at: OffsetDateTime,
    /// The snapshot the walk started from. Re-post it as `snapshot` to replay.
    pub(crate) snapshot: SnapshotBody,
    pub(crate) path: Vec<PathStepBody>,
    pub(crate) internal_effects: Vec<EffectBody>,
    pub(crate) external_effects: Vec<EffectBody>,
    pub(crate) waits: Vec<WaitBody>,
    pub(crate) wakes: Vec<WakeBody>,
    pub(crate) script: Vec<ScriptResultBody>,
    pub(crate) final_state: FinalStateBody,
    /// Set when the walk stopped early; the path up to there is still returned.
    pub(crate) halted: Option<HaltBody>,
    pub(crate) person_after: PersonAfterBody,
    /// Settings the walk cannot trigger, because a script has no segment membership changes:
    /// `goal_segment` (a goal of entering a segment), `exit_leaves_segment` (the criterion) and
    /// `exit_goal_reached_by_segment` (a `goal_reached` criterion when the goal is a segment).
    pub(crate) not_simulated: Vec<&'static str>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::macros::datetime;

    use super::*;

    #[test]
    fn a_snapshot_round_trips_through_the_evaluators_type_and_json() {
        let wire = json!({
            "traits": {"plan": "pro"},
            "events": [{"name": "order", "occurred_at": "2025-01-02T03:04:05Z",
                        "properties": {"total": 5}}],
            "capped_events": ["visit"],
            "events_complete_after": "2025-01-01T00:00:00Z",
            "lists": ["018f0000-0000-7000-8000-000000000001"],
            "consent": [{"channel": "email", "topic": null, "status": "subscribed"}],
            "identifier_kinds": ["email"],
            "segments": []
        });
        let body: SnapshotBody = serde_json::from_value(wire.clone()).unwrap();
        let snapshot = body.to_snapshot().unwrap();

        assert_eq!(
            snapshot.events[0].occurred_at,
            datetime!(2025-01-02 03:04:05 UTC)
        );
        assert!(snapshot.capped_events.contains("visit"));
        assert_eq!(SnapshotBody::of(&snapshot), body);
        assert_eq!(serde_json::to_value(&body).unwrap(), wire);
    }

    #[test]
    fn an_empty_snapshot_is_an_empty_object_and_an_unknown_key_is_refused() {
        let body: SnapshotBody = serde_json::from_value(json!({})).unwrap();

        assert_eq!(body, SnapshotBody::default());
        assert!(serde_json::from_value::<SnapshotBody>(json!({"trait": {}})).is_err());
    }

    #[test]
    fn an_unknown_consent_channel_or_status_is_a_422() {
        let mut body = SnapshotBody::default();
        body.consent.push(SnapshotConsentBody {
            channel: "fax".into(),
            topic: None,
            status: "subscribed".into(),
        });
        assert!(body.to_snapshot().is_err());
        body.consent[0].channel = "email".into();
        body.consent[0].status = "maybe".into();
        assert!(body.to_snapshot().is_err());
    }
}
