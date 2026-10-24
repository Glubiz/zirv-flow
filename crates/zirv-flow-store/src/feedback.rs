//! What a provider's verified feedback writes (architecture 10.2, #223): the `email.delivered`,
//! `email.bounced` and `email.complained` event of the message's person, the decisions under the
//! trace the message's enrollment began, and the suppression a hard bounce or a complaint makes.
//!
//! Nothing here holds an address. The address to suppress is the keyed hash
//! [`mark_message_sending`](crate::mark_message_sending) stored on the message when it went out
//! (supervisor ruling 86c6f5a4): feedback never reads the person's current address, so an address
//! the person changed since is never suppressed by mistake, and a message with no stored hash
//! (sent before the column existed) suppresses nobody and is reported as unable to.

use serde_json::{Map, Value, json};
use sqlx::PgPool;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::consent::SuppressionReason;
use zirv_flow_core::events::{EMAIL_BOUNCED_EVENT, EMAIL_COMPLAINED_EVENT, EMAIL_DELIVERED_EVENT};
use zirv_flow_core::{TraceContext, WorkspaceId};

use crate::flows::{Arrival, enqueue_arrivals};
use crate::tracking::enrollment_trace;
use crate::{
    NewDecision, NewSuppression, StoreError, add_organization_suppression, add_suppression,
    append_decisions, begin, commit,
};

/// The source of the events feedback writes.
const SOURCE: &str = "feedback";
/// The source of the suppressions feedback makes.
const SUPPRESSION_SOURCE: &str = "feedback";
/// The longest provider event id in a dedupe key; the API keeps it within this already.
const MAX_PROVIDER_EVENT_ID: usize = 128;

/// What the provider reported about a message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FeedbackEvent {
    Delivered,
    /// A hard bounce: the address cannot receive mail.
    BouncedHard,
    /// A soft bounce: a temporary failure. Suppresses nobody.
    BouncedSoft,
    Complained,
}

impl FeedbackEvent {
    /// The name of the event written.
    pub const fn event_name(self) -> &'static str {
        match self {
            Self::Delivered => EMAIL_DELIVERED_EVENT,
            Self::BouncedHard | Self::BouncedSoft => EMAIL_BOUNCED_EVENT,
            Self::Complained => EMAIL_COMPLAINED_EVENT,
        }
    }

    /// The reason of the decision that records the feedback.
    const fn reason(self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::BouncedHard => "bounced_hard",
            Self::BouncedSoft => "bounced_soft",
            Self::Complained => "complained",
        }
    }

    /// Whether the feedback asks for a suppression.
    const fn suppresses(self) -> bool {
        matches!(self, Self::BouncedHard | Self::Complained)
    }
}

/// One verified feedback event to record.
#[derive(Clone, Debug)]
pub struct ProviderFeedback<'a> {
    /// The workspace of the destination the webhook was received for.
    pub workspace: WorkspaceId,
    pub destination: Uuid,
    pub message: Uuid,
    pub event: FeedbackEvent,
    /// The provider's id of the event, when its manifest maps one: part of the dedupe key.
    pub provider_event_id: Option<&'a str>,
    pub now: OffsetDateTime,
    /// The trace to write under when the message's enrollment has none to give.
    pub fallback_trace: &'a TraceContext,
}

/// What recording feedback did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FeedbackOutcome {
    /// The event was written (unless the person is gone) and the suppression, if one was asked
    /// for, was made.
    Recorded {
        /// The event's id; none when the message's person was erased or merged away, in which
        /// case only the suppression was made.
        event: Option<Uuid>,
        suppression: SuppressionMade,
    },
    /// The same event of the same message was recorded before: nothing was written.
    Duplicate,
    /// No such message in the destination's workspace sent through this destination: nothing was
    /// written.
    UnknownMessage,
}

/// The suppression a feedback made.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SuppressionMade {
    /// The feedback asks for none (a delivery, a soft bounce).
    NotAsked,
    /// The address is suppressed in every workspace of the organization (a hard bounce).
    Organization,
    /// The address is suppressed in the workspace (a complaint).
    Workspace,
    /// A suppression was asked for and the message has no stored address hash: nobody was
    /// suppressed. The fallback to the person's current address is deliberately absent.
    Unable,
}

#[derive(FromRow)]
struct MessageRow {
    workspace_id: Uuid,
    destination_id: Uuid,
    person_id: Uuid,
    enrollment_id: Uuid,
    template_id: Uuid,
    address_hash: Option<Vec<u8>>,
}

/// `text` cut to at most `max` bytes, never inside a character.
fn truncated(text: &str, max: usize) -> &str {
    let end = text
        .char_indices()
        .map(|(at, _)| at)
        .chain(std::iter::once(text.len()))
        .take_while(|at| *at <= max)
        .last()
        .unwrap_or(0);
    &text[..end]
}

/// The dedupe key of a feedback event: the destination, the message, the event name and the
/// provider's event id when it has one. A provider that retries a webhook repeats all of them.
fn dedupe_key(feedback: &ProviderFeedback<'_>) -> String {
    let id = feedback
        .provider_event_id
        .map_or("", |id| truncated(id, MAX_PROVIDER_EVENT_ID));
    format!(
        "fb:{}:{}:{}:{}",
        feedback.destination,
        feedback.message,
        feedback.event.reason(),
        id
    )
}

/// Records one verified feedback event in a transaction of its own.
///
/// The message must exist in the destination's workspace and have been sent through the
/// destination; otherwise nothing is written ([`FeedbackOutcome::UnknownMessage`]). One event is
/// written per (message, event, provider event id): a provider's retry is
/// [`FeedbackOutcome::Duplicate`]. The event is written with `enqueue_arrivals`, so a flow that
/// waits for it wakes, and the decisions go under the trace the message's enrollment began, so
/// they sit in the trace of the send that caused them.
pub async fn record_provider_feedback(
    pool: &PgPool,
    feedback: &ProviderFeedback<'_>,
) -> Result<FeedbackOutcome, StoreError> {
    let mut tx = begin(pool).await?;
    let row: Option<MessageRow> = sqlx::query_as(
        "SELECT workspace_id, destination_id, person_id, enrollment_id, template_id, address_hash \
         FROM messages WHERE id = $1",
    )
    .bind(feedback.message)
    .fetch_optional(&mut *tx)
    .await
    .map_err(StoreError::query)?;
    let Some(row) = row.filter(|row| {
        row.workspace_id == feedback.workspace.as_uuid()
            && row.destination_id == feedback.destination
    }) else {
        return Ok(FeedbackOutcome::UnknownMessage);
    };

    // The person is locked for the write, like every event of a message: an erasure or a merge
    // that is running waits, and one that finished leaves no row to write an event for.
    let live: Option<Option<Uuid>> = sqlx::query_scalar(
        "SELECT merged_into FROM people WHERE workspace_id = $1 AND id = $2 FOR KEY SHARE",
    )
    .bind(row.workspace_id)
    .bind(row.person_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(StoreError::query)?;
    let person_live = matches!(live, Some(None));

    let trace = enrollment_trace(&mut tx, feedback.workspace, row.enrollment_id)
        .await?
        .unwrap_or_else(|| feedback.fallback_trace.clone());

    let key = dedupe_key(feedback);
    let event_id = Uuid::now_v7();
    if person_live {
        let first = sqlx::query(
            "INSERT INTO event_dedupe \
                (workspace_id, message_id, event_id, event_occurred_at, person_id) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (workspace_id, message_id) DO NOTHING",
        )
        .bind(row.workspace_id)
        .bind(&key)
        .bind(event_id)
        .bind(feedback.now)
        .bind(row.person_id)
        .execute(&mut *tx)
        .await
        .map_err(StoreError::query)?
        .rows_affected()
            == 1;
        if !first {
            sqlx::query(
                "UPDATE event_dedupe SET duplicates = duplicates + 1 \
                 WHERE workspace_id = $1 AND message_id = $2",
            )
            .bind(row.workspace_id)
            .bind(&key)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::query)?;
            commit(tx).await?;
            return Ok(FeedbackOutcome::Duplicate);
        }
        let mut properties = Map::new();
        properties.insert("message_id".to_owned(), json!(feedback.message));
        properties.insert("enrollment_id".to_owned(), json!(row.enrollment_id));
        properties.insert("template_id".to_owned(), json!(row.template_id));
        properties.insert("destination_id".to_owned(), json!(feedback.destination));
        if let FeedbackEvent::BouncedHard | FeedbackEvent::BouncedSoft = feedback.event {
            let kind = if feedback.event == FeedbackEvent::BouncedHard {
                "hard"
            } else {
                "soft"
            };
            properties.insert("kind".to_owned(), json!(kind));
            properties.insert("bounce".to_owned(), json!(kind));
        }
        sqlx::query(
            "INSERT INTO events (id, workspace_id, person_id, name, occurred_at, received_at, \
                properties, source, message_id, trace_id, validation_state) \
             VALUES ($1, $2, $3, $4, $5, $5, $6, $7, $8, $9, 'valid')",
        )
        .bind(event_id)
        .bind(row.workspace_id)
        .bind(row.person_id)
        .bind(feedback.event.event_name())
        .bind(feedback.now)
        .bind(Value::Object(properties))
        .bind(SOURCE)
        .bind(&key)
        .bind(trace.trace_id.as_uuid())
        .execute(&mut *tx)
        .await
        .map_err(StoreError::query)?;
        enqueue_arrivals(
            &mut tx,
            feedback.workspace,
            &[Arrival {
                event_id,
                person_id: row.person_id,
                name: feedback.event.event_name().to_owned(),
            }],
        )
        .await?;
    }

    let suppression = suppress(&mut tx, feedback, &row).await?;

    let ids = |extra: Option<(&'static str, Uuid)>| {
        let mut ids = vec![
            ("message", feedback.message),
            ("enrollment", row.enrollment_id),
            ("person", row.person_id),
            ("destination", feedback.destination),
        ];
        ids.extend(extra);
        ids
    };
    let mut decisions = vec![NewDecision {
        workspace_id: feedback.workspace,
        trace: trace.clone(),
        action: "message.feedback",
        outcome: "recorded",
        reason: Some(feedback.event.reason()),
        subject_ids: ids(person_live.then_some(("event", event_id))),
        job: None,
        graph: None,
    }];
    let (outcome, reason) = match suppression {
        SuppressionMade::NotAsked => None,
        SuppressionMade::Organization => Some(("added", "organization_hard_bounce")),
        SuppressionMade::Workspace => Some(("added", "workspace_complaint")),
        SuppressionMade::Unable => Some(("unable", "address_hash_missing")),
    }
    .unzip();
    if let (Some(outcome), Some(reason)) = (outcome, reason) {
        decisions.push(NewDecision {
            workspace_id: feedback.workspace,
            trace,
            action: "suppression.feedback",
            outcome,
            reason: Some(reason),
            subject_ids: ids(None),
            job: None,
            graph: None,
        });
    }
    append_decisions(&mut tx, &decisions).await?;
    commit(tx).await?;
    Ok(FeedbackOutcome::Recorded {
        event: person_live.then_some(event_id),
        suppression,
    })
}

/// Makes the suppression the feedback asks for, from the hash stored on the message.
async fn suppress(
    connection: &mut sqlx::PgConnection,
    feedback: &ProviderFeedback<'_>,
    row: &MessageRow,
) -> Result<SuppressionMade, StoreError> {
    if !feedback.event.suppresses() {
        return Ok(SuppressionMade::NotAsked);
    }
    let Some(hash) = row
        .address_hash
        .as_deref()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
    else {
        return Ok(SuppressionMade::Unable);
    };
    if feedback.event == FeedbackEvent::BouncedHard {
        // A hard bounce means the address does not exist for any brand (OQ-19).
        add_organization_suppression(
            &mut *connection,
            &NewSuppression {
                identifier_hash: hash,
                kind: "email",
                reason: SuppressionReason::HardBounce,
                source: SUPPRESSION_SOURCE,
                note: None,
                created_by: None,
            },
        )
        .await?;
        Ok(SuppressionMade::Organization)
    } else {
        // A complaint is about this workspace's mail: only this workspace stops sending.
        add_suppression(
            &mut *connection,
            feedback.workspace,
            &NewSuppression {
                identifier_hash: hash,
                kind: "email",
                reason: SuppressionReason::Complaint,
                source: SUPPRESSION_SOURCE,
                note: None,
                created_by: None,
            },
        )
        .await?;
        Ok(SuppressionMade::Workspace)
    }
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use time::macros::datetime;

    use super::*;
    use crate::flows::tests_support::{trace, workspace};
    use crate::{is_suppressed, suppressing_scope};

    const NOW: OffsetDateTime = datetime!(2031-05-06 07:08:09 UTC);

    #[test]
    fn a_dedupe_key_never_cuts_inside_a_multibyte_character() {
        let id = "\u{20ac}".repeat(100);
        let trace = TraceContext::root(zirv_flow_core::Cause::api("t"));
        let key = dedupe_key(&ProviderFeedback {
            workspace: WorkspaceId::new(),
            destination: Uuid::nil(),
            message: Uuid::nil(),
            event: FeedbackEvent::Delivered,
            provider_event_id: Some(&id),
            now: NOW,
            fallback_trace: &trace,
        });

        assert!(key.ends_with(&"\u{20ac}".repeat(42)));
        assert!(!key.ends_with(&"\u{20ac}".repeat(43)));
        assert_eq!(truncated("abc", 8), "abc");
        assert_eq!(truncated("\u{20ac}", 2), "");
    }

    struct Sent {
        person: Uuid,
        message: Uuid,
        destination: Uuid,
    }

    /// A person and one of their messages sent through `destination`, with `hash` as the
    /// recipient hash it was marked sending with.
    async fn sent(pool: &PgPool, ws: WorkspaceId, hash: Option<[u8; 32]>) -> Sent {
        let person = Uuid::now_v7();
        let destination = Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id, traits) VALUES ($1, $2, '{}')")
            .bind(person)
            .bind(ws.as_uuid())
            .execute(pool)
            .await
            .unwrap();
        let message = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO messages (id, workspace_id, person_id, enrollment_id, node_id, \
                template_id, destination_id, status, address_hash) \
             VALUES ($1, $2, $3, gen_random_uuid(), 'n', gen_random_uuid(), $4, 'pending', $5)",
        )
        .bind(message)
        .bind(ws.as_uuid())
        .bind(person)
        .bind(destination)
        .bind(hash.as_ref().map(|hash| hash.as_slice()))
        .execute(pool)
        .await
        .unwrap();
        Sent {
            person,
            message,
            destination,
        }
    }

    fn feedback<'a>(
        ws: WorkspaceId,
        sent: &Sent,
        event: FeedbackEvent,
        id: Option<&'a str>,
        trace: &'a TraceContext,
    ) -> ProviderFeedback<'a> {
        ProviderFeedback {
            workspace: ws,
            destination: sent.destination,
            message: sent.message,
            event,
            provider_event_id: id,
            now: NOW,
            fallback_trace: trace,
        }
    }

    async fn count(pool: &PgPool, sql: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[test]
    fn each_event_has_its_name_reason_and_whether_it_suppresses() {
        let all = [
            (FeedbackEvent::Delivered, "email.delivered", false),
            (FeedbackEvent::BouncedHard, "email.bounced", true),
            (FeedbackEvent::BouncedSoft, "email.bounced", false),
            (FeedbackEvent::Complained, "email.complained", true),
        ];
        for (event, name, suppresses) in all {
            assert_eq!(event.event_name(), name);
            assert_eq!(event.suppresses(), suppresses);
        }
        let mut reasons: Vec<_> = all.iter().map(|(event, ..)| event.reason()).collect();
        reasons.sort_unstable();
        reasons.dedup();
        assert_eq!(reasons.len(), 4);
    }

    #[test]
    fn the_dedupe_key_names_destination_message_event_and_provider_id() {
        let trace = zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("t"));
        let ws = WorkspaceId::from_uuid(Uuid::from_u128(1));
        let sent = Sent {
            person: Uuid::from_u128(2),
            message: Uuid::from_u128(3),
            destination: Uuid::from_u128(4),
        };
        let with = feedback(ws, &sent, FeedbackEvent::Delivered, Some("evt-9"), &trace);
        let without = feedback(ws, &sent, FeedbackEvent::Delivered, None, &trace);
        let other = feedback(ws, &sent, FeedbackEvent::Complained, Some("evt-9"), &trace);

        assert!(dedupe_key(&with).ends_with(":delivered:evt-9"));
        assert!(dedupe_key(&without).ends_with(":delivered:"));
        assert_ne!(dedupe_key(&with), dedupe_key(&other));
        assert!(!dedupe_key(&with).contains('@'));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_hard_bounce_writes_the_event_and_suppresses_the_stored_hash_in_the_organization(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "fb-hard").await;
        let other = workspace(&pool, "fb-other").await;
        let hash = [9_u8; 32];
        let sent = sent(&pool, ws, Some(hash)).await;
        let trace = trace();

        let outcome = record_provider_feedback(
            &pool,
            &feedback(ws, &sent, FeedbackEvent::BouncedHard, Some("e-1"), &trace),
        )
        .await
        .unwrap();

        let FeedbackOutcome::Recorded { event, suppression } = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(suppression, SuppressionMade::Organization);
        let (name, person, props, source): (String, Uuid, Value, String) =
            sqlx::query_as("SELECT name, person_id, properties, source FROM events WHERE id = $1")
                .bind(event.unwrap())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            (name.as_str(), person, source.as_str()),
            ("email.bounced", sent.person, "feedback")
        );
        assert_eq!(props["kind"], json!("hard"));
        assert_eq!(props["message_id"], json!(sent.message));
        assert!(!props.to_string().contains('@'));
        assert!(is_suppressed(&pool, ws, &hash).await.unwrap());
        assert!(
            is_suppressed(&pool, other, &hash).await.unwrap(),
            "a hard bounce suppresses in every workspace"
        );
        let scope = suppressing_scope(&pool, other, &hash).await.unwrap();
        assert!(scope.is_some());
        assert_eq!(
            count(&pool, "SELECT count(*) FROM suppressions WHERE scope = 'organization' AND reason = 'hard_bounce' AND source = 'feedback'").await,
            1
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_complaint_suppresses_only_its_workspace(pool: PgPool) {
        let ws = workspace(&pool, "fb-complaint").await;
        let other = workspace(&pool, "fb-complaint-other").await;
        let hash = [4_u8; 32];
        let sent = sent(&pool, ws, Some(hash)).await;
        let trace = trace();

        let outcome = record_provider_feedback(
            &pool,
            &feedback(ws, &sent, FeedbackEvent::Complained, None, &trace),
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome,
            FeedbackOutcome::Recorded {
                suppression: SuppressionMade::Workspace,
                ..
            }
        ));
        assert!(is_suppressed(&pool, ws, &hash).await.unwrap());
        assert!(!is_suppressed(&pool, other, &hash).await.unwrap());
        assert_eq!(
            count(&pool, "SELECT count(*) FROM suppressions WHERE scope = 'workspace' AND reason = 'complaint'").await,
            1
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_delivery_and_a_soft_bounce_suppress_nobody(pool: PgPool) {
        let ws = workspace(&pool, "fb-soft").await;
        let sent = sent(&pool, ws, Some([1_u8; 32])).await;
        let trace = trace();

        for (event, id) in [
            (FeedbackEvent::Delivered, "a"),
            (FeedbackEvent::BouncedSoft, "b"),
        ] {
            let outcome =
                record_provider_feedback(&pool, &feedback(ws, &sent, event, Some(id), &trace))
                    .await
                    .unwrap();
            assert!(matches!(
                outcome,
                FeedbackOutcome::Recorded {
                    suppression: SuppressionMade::NotAsked,
                    ..
                }
            ));
        }

        assert_eq!(count(&pool, "SELECT count(*) FROM suppressions").await, 0);
        assert_eq!(count(&pool, "SELECT count(*) FROM events").await, 2);
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM events WHERE properties ->> 'kind' = 'soft'"
            )
            .await,
            1
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_message_without_a_stored_hash_writes_the_event_but_suppresses_nobody(pool: PgPool) {
        let ws = workspace(&pool, "fb-null").await;
        let sent = sent(&pool, ws, None).await;
        // The person's current address must not be used as a fallback.
        sqlx::query("UPDATE people SET traits = '{\"email\": \"now@example.test\"}'")
            .execute(&pool)
            .await
            .unwrap();
        let trace = trace();

        let outcome = record_provider_feedback(
            &pool,
            &feedback(ws, &sent, FeedbackEvent::BouncedHard, Some("e"), &trace),
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome,
            FeedbackOutcome::Recorded {
                event: Some(_),
                suppression: SuppressionMade::Unable
            }
        ));
        assert_eq!(count(&pool, "SELECT count(*) FROM suppressions").await, 0);
        assert_eq!(
            count(&pool, "SELECT count(*) FROM decision_records WHERE action = 'suppression.feedback' AND outcome = 'unable' AND reason = 'address_hash_missing'").await,
            1
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_provider_retry_of_the_same_feedback_writes_one_event_and_counts_the_repeat(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "fb-dup").await;
        let sent = sent(&pool, ws, Some([2_u8; 32])).await;
        let trace = trace();
        let first = feedback(ws, &sent, FeedbackEvent::BouncedHard, Some("e-7"), &trace);

        let one = record_provider_feedback(&pool, &first).await.unwrap();
        let again = record_provider_feedback(&pool, &first).await.unwrap();
        let other_id = record_provider_feedback(
            &pool,
            &feedback(ws, &sent, FeedbackEvent::BouncedHard, Some("e-8"), &trace),
        )
        .await
        .unwrap();

        assert!(matches!(one, FeedbackOutcome::Recorded { .. }));
        assert_eq!(again, FeedbackOutcome::Duplicate);
        assert!(
            matches!(other_id, FeedbackOutcome::Recorded { .. }),
            "another provider event id is another event"
        );
        assert_eq!(count(&pool, "SELECT count(*) FROM events").await, 2);
        assert_eq!(
            count(
                &pool,
                "SELECT duplicates::bigint FROM event_dedupe WHERE message_id LIKE '%e-7'"
            )
            .await,
            1
        );
        assert_eq!(count(&pool, "SELECT count(*) FROM suppressions").await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_unknown_message_another_workspace_or_another_destination_writes_nothing(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "fb-unknown").await;
        let other = workspace(&pool, "fb-unknown-other").await;
        let sent = sent(&pool, ws, Some([3_u8; 32])).await;
        let trace = trace();
        let mut ghost = feedback(ws, &sent, FeedbackEvent::BouncedHard, None, &trace);
        ghost.message = Uuid::now_v7();
        let mut elsewhere = feedback(other, &sent, FeedbackEvent::BouncedHard, None, &trace);
        elsewhere.destination = sent.destination;
        let mut wrong_destination = feedback(ws, &sent, FeedbackEvent::BouncedHard, None, &trace);
        wrong_destination.destination = Uuid::now_v7();

        for case in [ghost, elsewhere, wrong_destination] {
            assert_eq!(
                record_provider_feedback(&pool, &case).await.unwrap(),
                FeedbackOutcome::UnknownMessage
            );
        }

        for table in ["events", "suppressions", "decision_records", "event_dedupe"] {
            assert_eq!(
                count(&pool, &format!("SELECT count(*) FROM {table}")).await,
                0,
                "{table}"
            );
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_decision_sits_under_the_enrollments_trace_with_its_original_cause(pool: PgPool) {
        let ws = workspace(&pool, "fb-trace").await;
        let sent = sent(&pool, ws, Some([5_u8; 32])).await;
        let fallback = trace();

        record_provider_feedback(
            &pool,
            &feedback(ws, &sent, FeedbackEvent::Complained, None, &fallback),
        )
        .await
        .unwrap();

        // The enrollment is gone in this fixture: the fallback trace is used.
        let rows: Vec<(Uuid, String, String)> = sqlx::query_as(
            "SELECT trace_id, action, cause_kind FROM decision_records ORDER BY action",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|(trace, ..)| *trace == fallback.trace_id.as_uuid())
        );
        assert_eq!(rows[0].1, "message.feedback");
        assert_eq!(rows[1].1, "suppression.feedback");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn nothing_in_a_row_event_or_decision_holds_an_address(pool: PgPool) {
        let ws = workspace(&pool, "fb-noleak").await;
        let sent = sent(&pool, ws, Some([6_u8; 32])).await;
        let trace = trace();

        record_provider_feedback(
            &pool,
            &feedback(ws, &sent, FeedbackEvent::BouncedHard, Some("e-1"), &trace),
        )
        .await
        .unwrap();

        for sql in [
            "SELECT coalesce(string_agg(properties::text, ' '), '') FROM events",
            "SELECT coalesce(string_agg(subject_ids::text || coalesce(reason, ''), ' '), '') FROM decision_records",
            "SELECT coalesce(string_agg(identifier_hash::text || source || coalesce(note, ''), ' '), '') FROM suppressions",
            "SELECT coalesce(string_agg(message_id, ' '), '') FROM event_dedupe",
        ] {
            let text: String = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
                .fetch_one(&pool)
                .await
                .unwrap();
            assert!(!text.contains('@'), "{sql}: {text}");
        }
    }
}
