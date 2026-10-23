//! The payload of a `delivery.send` job: what `flow.step` enqueues for an `action.send` node, and
//! what a deferred send carries to its next run.
//!
//! It holds ids, the node's consent coordinates and two times, never an address or a trait: the
//! address is read when the job runs. `person_id` is in it so that erasing the person finds and
//! purges the job (`names_erased_person!` in the store).

use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::consent::Dimensions;

/// What `flow.step` knows when a send node runs.
pub(crate) struct Queued<'a> {
    pub(crate) workspace: WorkspaceId,
    /// The message id, minted for this send. It is the idempotency key at the destination across
    /// every attempt, deferral and retry.
    pub(crate) message: Uuid,
    /// The stable key of the external effect (`{enrollment}:{node}:send`), which is also the
    /// job's unique key.
    pub(crate) key: &'a str,
    pub(crate) flow: Uuid,
    pub(crate) version: i32,
    pub(crate) node: &'a str,
    pub(crate) enrollment: Uuid,
    pub(crate) person: Uuid,
    pub(crate) template: Uuid,
    pub(crate) destination: Uuid,
    pub(crate) topic: Option<Uuid>,
    pub(crate) dimensions: &'a Dimensions,
    pub(crate) queued_at: OffsetDateTime,
}

impl Queued<'_> {
    /// The payload of the job. `None` only when the time cannot be formatted.
    pub(crate) fn payload(&self) -> Option<Value> {
        let mut payload = json!({
            "workspace_id": self.workspace,
            "message_id": self.message,
            "effect_key": self.key,
            "flow_id": self.flow,
            "version": self.version,
            "node_id": self.node,
            "enrollment_id": self.enrollment,
            "person_id": self.person,
            "template_id": self.template,
            "destination_id": self.destination,
            "dimensions": self.dimensions,
            "queued_at": self.queued_at.format(&Rfc3339).ok()?,
            "deferrals": 0,
            "attempt_base": 0,
        });
        // Only a node that names a topic carries one.
        if let (Some(topic), Some(map)) = (self.topic, payload.as_object_mut()) {
            map.insert("topic_id".to_owned(), json!(topic));
        }
        Some(payload)
    }
}

/// A send job's payload, parsed.
#[derive(Clone, Debug)]
pub(super) struct SendJob {
    pub(super) workspace: WorkspaceId,
    pub(super) message: Uuid,
    pub(super) key: String,
    pub(super) flow: Uuid,
    pub(super) version: i32,
    pub(super) node: String,
    pub(super) enrollment: Uuid,
    pub(super) person: Uuid,
    pub(super) template: Uuid,
    pub(super) destination: Uuid,
    pub(super) topic: Option<Uuid>,
    pub(super) dimensions: Dimensions,
    /// How many times the send waited for a rate-limit token and was queued again.
    pub(super) deferrals: u32,
    /// When the send first waited for a token; none before its first deferral. The six hours a
    /// send may wait are counted from here, not from when it was queued: a backed-up queue is
    /// not the destination's fault.
    pub(super) first_wait: Option<OffsetDateTime>,
    /// The delivery attempts earlier runs of this send made, which an attempt number continues
    /// from: a deferral starts a new job whose own attempts count from one.
    pub(super) attempt_base: i32,
}

impl SendJob {
    pub(super) fn parse(payload: &Value) -> Option<Self> {
        let text = |key: &str| payload.get(key).and_then(Value::as_str);
        let id = |key: &str| text(key).and_then(|text| Uuid::parse_str(text).ok());
        let topic = match payload.get("topic_id") {
            None | Some(Value::Null) => None,
            Some(value) => Some(Uuid::parse_str(value.as_str()?).ok()?),
        };
        let dimensions = payload
            .get("dimensions")?
            .as_object()?
            .iter()
            .map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
            .collect::<Option<Dimensions>>()?;
        let queued_at = OffsetDateTime::parse(text("queued_at")?, &Rfc3339).ok()?;
        let deferrals = u32::try_from(payload.get("deferrals")?.as_u64()?).ok()?;
        Some(Self {
            workspace: text("workspace_id")?.parse().ok()?,
            message: id("message_id")?,
            key: text("effect_key")?.to_owned(),
            flow: id("flow_id")?,
            version: i32::try_from(payload.get("version")?.as_i64()?).ok()?,
            node: text("node_id")?.to_owned(),
            enrollment: id("enrollment_id")?,
            person: id("person_id")?,
            template: id("template_id")?,
            destination: id("destination_id")?,
            topic,
            dimensions,
            deferrals,
            first_wait: match payload.get("first_wait") {
                // A payload queued again before the first wait was stored: it waited since it was
                // queued.
                None | Some(Value::Null) => (deferrals > 0).then_some(queued_at),
                Some(value) => Some(OffsetDateTime::parse(value.as_str()?, &Rfc3339).ok()?),
            },
            attempt_base: i32::try_from(payload.get("attempt_base")?.as_i64()?).ok()?,
        })
    }

    /// The payload of the job that carries this send on after a deferral in attempt `attempts` of
    /// the job holding `payload`: one deferral more, the attempts the delivery has made so far, and
    /// the time of the first wait (`now` on the first deferral, carried over after).
    pub(super) fn deferred(&self, payload: &Value, attempts: i32, now: OffsetDateTime) -> Value {
        let mut next = payload.clone();
        if let Some(map) = next.as_object_mut() {
            map.insert("deferrals".to_owned(), json!(self.deferrals + 1));
            if let Ok(first) = self.first_wait.unwrap_or(now).format(&Rfc3339) {
                map.insert("first_wait".to_owned(), json!(first));
            }
            map.insert(
                "attempt_base".to_owned(),
                json!(self.attempt_base + attempts - 1),
            );
        }
        next
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use time::macros::datetime;

    use super::*;

    fn queued(topic: Option<Uuid>, dimensions: &Dimensions) -> Queued<'_> {
        Queued {
            workspace: WorkspaceId::from_uuid(Uuid::from_u128(1)),
            message: Uuid::from_u128(2),
            key: "e:mail:send",
            flow: Uuid::from_u128(3),
            version: 4,
            node: "mail",
            enrollment: Uuid::from_u128(5),
            person: Uuid::from_u128(6),
            template: Uuid::from_u128(7),
            destination: Uuid::from_u128(8),
            topic,
            dimensions,
            queued_at: datetime!(2031-05-06 07:08:09 UTC),
        }
    }

    #[test]
    fn a_queued_payload_parses_back_and_names_the_person_but_holds_no_address() {
        let dimensions = BTreeMap::from([("vertical".to_owned(), "sports".to_owned())]);
        let topic = Uuid::from_u128(9);

        let payload = queued(Some(topic), &dimensions).payload().unwrap();
        let parsed = SendJob::parse(&payload).expect("it parses");

        assert_eq!(parsed.message, Uuid::from_u128(2));
        assert_eq!(parsed.person, Uuid::from_u128(6));
        assert_eq!(parsed.topic, Some(topic));
        assert_eq!(parsed.dimensions, dimensions);
        assert_eq!((parsed.deferrals, parsed.attempt_base), (0, 0));
        assert!(!payload.to_string().contains('@'));
        assert!(
            queued(None, &dimensions)
                .payload()
                .unwrap()
                .get("topic_id")
                .is_none()
        );
    }

    #[test]
    fn a_payload_with_a_missing_or_mistyped_field_is_not_a_send() {
        let dimensions = Dimensions::new();
        let whole = queued(None, &dimensions).payload().unwrap();
        for field in [
            "workspace_id",
            "message_id",
            "effect_key",
            "flow_id",
            "version",
            "node_id",
            "enrollment_id",
            "person_id",
            "template_id",
            "destination_id",
            "dimensions",
            "queued_at",
            "deferrals",
            "attempt_base",
        ] {
            let mut broken = whole.clone();
            broken.as_object_mut().unwrap().remove(field);
            assert!(SendJob::parse(&broken).is_none(), "{field}");
        }
        let mut broken = whole.clone();
        broken["dimensions"] = json!({"vertical": 7});
        assert!(SendJob::parse(&broken).is_none());
        let mut broken = whole;
        broken["topic_id"] = json!("not an id");
        assert!(SendJob::parse(&broken).is_none());
    }

    #[test]
    fn a_deferral_counts_itself_and_carries_the_attempts_made_so_far() {
        let dimensions = Dimensions::new();
        let payload = queued(None, &dimensions).payload().unwrap();
        let job = SendJob::parse(&payload).unwrap();

        let t1 = datetime!(2031-05-07 00:00:00 UTC);
        let t2 = datetime!(2031-05-07 03:00:00 UTC);
        let first = job.deferred(&payload, 3, t1);
        let second = SendJob::parse(&first).unwrap().deferred(&first, 1, t2);

        let first_job = SendJob::parse(&first).unwrap();
        assert_eq!((first_job.deferrals, first_job.attempt_base), (1, 2));
        assert_eq!(job.first_wait, None);
        assert_eq!(first_job.first_wait, Some(t1));
        assert_eq!(
            SendJob::parse(&second).unwrap().first_wait,
            Some(t1),
            "the first wait is carried over, not moved"
        );
        let second_job = SendJob::parse(&second).unwrap();
        assert_eq!((second_job.deferrals, second_job.attempt_base), (2, 2));
        assert_eq!(
            first_job.message, job.message,
            "the message id never changes"
        );
    }
}
