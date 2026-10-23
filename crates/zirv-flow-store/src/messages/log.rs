//! The `messages` rows: one per message the send pipeline decided on.

use std::str::FromStr;

use serde_json::{Value, json};
use sqlx::PgExecutor;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::consent::Dimensions;

use crate::StoreError;

/// Where a message is: in the pipeline, or how it ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageStatus {
    /// The job has started on it and it has not ended: it may be waiting for a rate-limit token or
    /// for a retry.
    Pending,
    /// The destination is being called, or was: a retry must not judge the message again by the
    /// gates, since it may have gone out.
    Sending,
    /// Handed to the destination, which accepted it.
    Sent,
    /// A gate stopped it before anything was rendered or sent.
    Skipped,
    /// It could not be rendered or delivered.
    Failed,
}

impl MessageStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Sending => "sending",
            Self::Sent => "sent",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
        }
    }
}

impl MessageStatus {
    /// Whether the message has ended: sent, skipped or failed.
    pub const fn has_ended(self) -> bool {
        !matches!(self, Self::Pending | Self::Sending)
    }
}

impl FromStr for MessageStatus {
    type Err = StoreError;

    fn from_str(text: &str) -> Result<Self, StoreError> {
        match text {
            "pending" => Ok(Self::Pending),
            "sending" => Ok(Self::Sending),
            "sent" => Ok(Self::Sent),
            "skipped" => Ok(Self::Skipped),
            "failed" => Ok(Self::Failed),
            _ => Err(StoreError::query(sqlx::Error::Decode(
                "unknown message status".into(),
            ))),
        }
    }
}

/// What one gate of the pipeline found. Codes and ids only: never an address, a trait value or a
/// destination's answer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GateRecord {
    /// The gate: `enrollment`, `address`, `consent`, `suppression`, `sender`, `frequency_cap`,
    /// `quiet_hours`, `render`, `rate_limit` or `delivery`.
    pub gate: &'static str,
    pub passed: bool,
    /// Why it blocked: a skip or failure code; none when it passed.
    pub reason: Option<&'static str>,
    /// The id the gate's finding rests on, such as the consent change that decided or the scope
    /// of the suppression that blocked, as `(name, id)`.
    pub detail: Option<(&'static str, Uuid)>,
    /// A code that adds to the reason, such as what a message that ended unconfirmed really ran
    /// into. A code, never an address.
    pub note: Option<&'static str>,
}

impl GateRecord {
    /// A gate the message passed.
    pub const fn passed(gate: &'static str) -> Self {
        Self {
            gate,
            passed: true,
            reason: None,
            detail: None,
            note: None,
        }
    }

    /// A gate that stopped the message for `reason`.
    pub const fn blocked(gate: &'static str, reason: &'static str) -> Self {
        Self {
            gate,
            passed: false,
            reason: Some(reason),
            detail: None,
            note: None,
        }
    }

    /// The same record, naming the id its finding rests on.
    pub const fn with_detail(mut self, name: &'static str, id: Uuid) -> Self {
        self.detail = Some((name, id));
        self
    }

    /// The same record, with a code that says more than the reason does.
    pub const fn with_note(mut self, note: &'static str) -> Self {
        self.note = Some(note);
        self
    }

    fn to_value(self) -> Value {
        let mut entry = json!({
            "gate": self.gate,
            "result": if self.passed { "passed" } else { "blocked" },
        });
        if let Some(reason) = self.reason {
            entry["reason"] = json!(reason);
        }
        if let Some((name, id)) = self.detail {
            entry[name] = json!(id);
        }
        if let Some(note) = self.note {
            entry["note"] = json!(note);
        }
        entry
    }
}

/// A message to record. The id was minted when the send was queued and is the idempotency key
/// the destination saw.
#[derive(Clone, Debug)]
pub struct NewMessage<'a> {
    pub id: Uuid,
    pub workspace: WorkspaceId,
    pub person: Uuid,
    pub enrollment: Uuid,
    pub flow: Option<Uuid>,
    pub flow_version: Option<i32>,
    pub node: &'a str,
    pub template: Uuid,
    pub template_version: Option<i32>,
    pub destination: Uuid,
    pub sender_identity: Option<Uuid>,
    pub category: Option<&'a str>,
    pub topic: Option<Uuid>,
    pub dimensions: &'a Dimensions,
    pub status: MessageStatus,
    /// A fixed code; required for a skipped or failed message, absent for a sent one.
    pub reason: Option<&'static str>,
    /// What decided the consent gate: `recorded_consent`, `topic_default` or `none`.
    pub deciding_rule: Option<&'static str>,
    pub gates: &'a [GateRecord],
    pub attempts: i32,
    pub now: OffsetDateTime,
}

/// A message as stored.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct Message {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub person_id: Uuid,
    pub enrollment_id: Uuid,
    pub flow_id: Option<Uuid>,
    pub flow_version: Option<i32>,
    pub node_id: String,
    pub template_id: Uuid,
    pub template_version: Option<i32>,
    pub destination_id: Uuid,
    pub sender_identity_id: Option<Uuid>,
    pub category: Option<String>,
    pub channel: String,
    pub topic_id: Option<Uuid>,
    pub dimensions: Value,
    pub status: String,
    pub reason: Option<String>,
    pub deciding_rule: Option<String>,
    pub gate_results: Value,
    pub test: bool,
    pub attempts: i32,
    pub created_at: OffsetDateTime,
    pub completed_at: Option<OffsetDateTime>,
}

pub(crate) const COLUMNS: &str = "id, workspace_id, person_id, enrollment_id, flow_id, flow_version, \
    node_id, template_id, template_version, destination_id, sender_identity_id, category, \
    channel, topic_id, dimensions, status, reason, deciding_rule, gate_results, test, attempts, \
    created_at, completed_at";

/// Writes the message and says whether this call changed it.
///
/// A message is written twice: `pending` when the send job first runs, and again with how it
/// ended. So a row that does not exist is inserted, a `pending` one is ended by an ended `new`,
/// and anything else is left alone, which is false: a pending row is not touched by a second
/// start, and an ended one is never changed, so the job that committed its outcome and ran again
/// changes nothing.
pub async fn insert_message<'e>(
    executor: impl PgExecutor<'e>,
    new: &NewMessage<'_>,
) -> Result<bool, StoreError> {
    let gates: Vec<Value> = new.gates.iter().map(|gate| gate.to_value()).collect();
    let dimensions = json!(new.dimensions);
    let completed = new.status.has_ended().then_some(new.now);
    let written = sqlx::query(
        "INSERT INTO messages             (id, workspace_id, person_id, enrollment_id, flow_id, flow_version, node_id,              template_id, template_version, destination_id, sender_identity_id, category,              topic_id, dimensions, status, reason, deciding_rule, gate_results, attempts,              created_at, completed_at)          VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17,                  $18, $19, $20, $21)          ON CONFLICT (id) DO UPDATE SET             template_version = EXCLUDED.template_version,             sender_identity_id = EXCLUDED.sender_identity_id, category = EXCLUDED.category,             status = EXCLUDED.status, reason = EXCLUDED.reason,             deciding_rule = EXCLUDED.deciding_rule, gate_results = EXCLUDED.gate_results,             attempts = EXCLUDED.attempts, completed_at = EXCLUDED.completed_at          WHERE messages.status IN ('pending', 'sending') \n            AND EXCLUDED.status NOT IN ('pending', 'sending')",
    )
    .bind(new.id)
    .bind(new.workspace.as_uuid())
    .bind(new.person)
    .bind(new.enrollment)
    .bind(new.flow)
    .bind(new.flow_version)
    .bind(new.node)
    .bind(new.template)
    .bind(new.template_version)
    .bind(new.destination)
    .bind(new.sender_identity)
    .bind(new.category)
    .bind(new.topic)
    .bind(dimensions)
    .bind(new.status.as_str())
    .bind(new.reason)
    .bind(new.deciding_rule)
    .bind(Value::Array(gates))
    .bind(new.attempts)
    .bind(new.now)
    .bind(completed)
    .execute(executor)
    .await
    .map_err(StoreError::query)?
    .rows_affected();
    Ok(written == 1)
}

/// Marks the message `sending` before the destination is called, in a statement of its own, so
/// the mark survives a job transaction that rolls back. A message that has ended is left alone;
/// false then.
pub async fn mark_message_sending<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
    attempts: i32,
) -> Result<bool, StoreError> {
    let marked = sqlx::query(
        "UPDATE messages SET status = 'sending', attempts = $3          WHERE workspace_id = $1 AND id = $2 AND status IN ('pending', 'sending')",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(attempts)
    .execute(executor)
    .await
    .map_err(StoreError::query)?
    .rows_affected();
    Ok(marked == 1)
}

/// The gate entry that notes an attempt lost the mail server's reply after the end of the message.
fn unconfirmed_note() -> Value {
    json!([{"gate": "delivery", "result": "unconfirmed"}])
}

/// Notes on a `sending` message that an attempt sent the whole message and then lost the server's
/// reply, so it may have been accepted. The note is a `delivery` entry in `gate_results`, written
/// in a statement of its own so it survives a job retry, a deferral and a lost lease; the write
/// that ends the message replaces the gate results. False when the message is not `sending` or
/// already carries the note.
pub async fn mark_message_unconfirmed<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<bool, StoreError> {
    let marked = sqlx::query(
        "UPDATE messages SET gate_results = gate_results || $3          WHERE workspace_id = $1 AND id = $2 AND status = 'sending'            AND NOT gate_results @> $3",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(unconfirmed_note())
    .execute(executor)
    .await
    .map_err(StoreError::query)?
    .rows_affected();
    Ok(marked == 1)
}

/// Whether a `sending` message carries the note of [`mark_message_unconfirmed`].
pub async fn message_unconfirmed<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<bool, StoreError> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM messages WHERE workspace_id = $1 AND id = $2              AND status = 'sending' AND gate_results @> $3)",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(unconfirmed_note())
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)
}

/// The message with this id in the workspace.
pub async fn find_message<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<Message>, StoreError> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM messages WHERE workspace_id = $1 AND id = $2"
    )))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// Where the message with this id is, or none when no row was written for it yet.
pub async fn message_status<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<MessageStatus>, StoreError> {
    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM messages WHERE workspace_id = $1 AND id = $2")
            .bind(workspace.as_uuid())
            .bind(id)
            .fetch_optional(executor)
            .await
            .map_err(StoreError::query)?;
    status.as_deref().map(str::parse).transpose()
}

/// A person's messages, newest first, at most `limit`. Messages stay on the id they were written
/// for when people merge, so the log reads through `person_merges`: the person's own and those
/// of everyone merged into them, directly or through other merges.
pub async fn list_person_messages<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    person: Uuid,
    limit: u32,
) -> Result<Vec<Message>, StoreError> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "WITH RECURSIVE who(id) AS ( \
            SELECT $2::uuid \
            UNION \
            SELECT m.merged_person_id FROM person_merges m JOIN who ON m.survivor_person_id = who.id \
            WHERE m.workspace_id = $1) \
         SELECT {COLUMNS} FROM messages \
         WHERE workspace_id = $1 AND person_id IN (SELECT id FROM who) \
         ORDER BY created_at DESC, id DESC LIMIT $3"
    )))
    .bind(workspace.as_uuid())
    .bind(person)
    .bind(i64::from(limit))
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_round_trips_through_its_text_and_an_unknown_one_is_refused() {
        for status in [
            MessageStatus::Pending,
            MessageStatus::Sent,
            MessageStatus::Skipped,
            MessageStatus::Failed,
        ] {
            assert_eq!(status.as_str().parse::<MessageStatus>().unwrap(), status);
        }
        assert!("queued".parse::<MessageStatus>().is_err());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_message_is_written_once_and_the_log_reads_through_merges(pool: sqlx::PgPool) {
        use zirv_flow_core::{Slug, WorkspaceName};
        let ws = crate::create_workspace(
            &pool,
            &WorkspaceName::parse("one").unwrap(),
            &Slug::parse("one").unwrap(),
        )
        .await
        .unwrap()
        .id;
        let mut people = Vec::new();
        for _ in 0..3 {
            let id = Uuid::now_v7();
            sqlx::query("INSERT INTO people (id, workspace_id, traits) VALUES ($1, $2, '{}')")
                .bind(id)
                .bind(ws.as_uuid())
                .execute(&pool)
                .await
                .unwrap();
            people.push(id);
        }
        let dimensions = Dimensions::new();
        let mut connection = pool.acquire().await.unwrap();
        let mut ids = Vec::new();
        for (index, person) in people.iter().enumerate() {
            let id = Uuid::now_v7();
            ids.push(id);
            let new = NewMessage {
                id,
                workspace: ws,
                person: *person,
                enrollment: Uuid::now_v7(),
                flow: None,
                flow_version: None,
                node: "mail",
                template: Uuid::from_u128(1),
                template_version: Some(1),
                destination: Uuid::from_u128(2),
                sender_identity: None,
                category: Some("transactional"),
                topic: None,
                dimensions: &dimensions,
                status: MessageStatus::Sent,
                reason: None,
                deciding_rule: None,
                gates: &[],
                attempts: 1,
                now: OffsetDateTime::now_utc() + time::Duration::seconds(index as i64),
            };
            assert!(insert_message(&mut *connection, &new).await.unwrap());
            assert!(
                !insert_message(&mut *connection, &new).await.unwrap(),
                "the same id is not written twice"
            );
        }
        assert_eq!(
            message_status(&pool, ws, ids[0]).await.unwrap(),
            Some(MessageStatus::Sent)
        );
        assert_eq!(
            message_status(&pool, ws, Uuid::now_v7()).await.unwrap(),
            None
        );
        // people[0] was merged into people[1], which was merged into people[2].
        for (survivor, merged) in [(people[1], people[0]), (people[2], people[1])] {
            sqlx::query(
                "INSERT INTO person_merges \
                    (id, workspace_id, survivor_person_id, merged_person_id, trace_id) \
                 VALUES ($1, $2, $3, $4, gen_random_uuid())",
            )
            .bind(Uuid::now_v7())
            .bind(ws.as_uuid())
            .bind(survivor)
            .bind(merged)
            .execute(&pool)
            .await
            .unwrap();
        }

        let survivor = list_person_messages(&pool, ws, people[2], 10)
            .await
            .unwrap();
        let middle = list_person_messages(&pool, ws, people[1], 10)
            .await
            .unwrap();
        let limited = list_person_messages(&pool, ws, people[2], 1).await.unwrap();

        assert_eq!(
            survivor
                .iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            [ids[2], ids[1], ids[0]],
            "newest first, every merged person's included"
        );
        assert_eq!(middle.len(), 2);
        assert_eq!(limited.len(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_pending_message_is_ended_once_and_an_ended_one_never_changes(pool: sqlx::PgPool) {
        use zirv_flow_core::{Slug, WorkspaceName};
        let ws = crate::create_workspace(
            &pool,
            &WorkspaceName::parse("one").unwrap(),
            &Slug::parse("one").unwrap(),
        )
        .await
        .unwrap()
        .id;
        let person = Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id, traits) VALUES ($1, $2, '{}')")
            .bind(person)
            .bind(ws.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        let dimensions = Dimensions::new();
        let id = Uuid::now_v7();
        let message = |status, reason, attempts| NewMessage {
            id,
            workspace: ws,
            person,
            enrollment: Uuid::now_v7(),
            flow: None,
            flow_version: None,
            node: "mail",
            template: Uuid::from_u128(1),
            template_version: None,
            destination: Uuid::from_u128(2),
            sender_identity: None,
            category: None,
            topic: None,
            dimensions: &dimensions,
            status,
            reason,
            deciding_rule: None,
            gates: &[],
            attempts,
            now: OffsetDateTime::now_utc(),
        };

        let started = insert_message(&pool, &message(MessageStatus::Pending, None, 0)).await;
        let again = insert_message(&pool, &message(MessageStatus::Pending, None, 0)).await;
        let pending = find_message(&pool, ws, id).await.unwrap().unwrap();
        let marked = mark_message_sending(&pool, ws, id, 1).await;
        let sending = find_message(&pool, ws, id).await.unwrap().unwrap();
        let ended = insert_message(
            &pool,
            &message(MessageStatus::Failed, Some("render_error"), 2),
        )
        .await;
        let late = insert_message(&pool, &message(MessageStatus::Sent, None, 3)).await;
        let found = find_message(&pool, ws, id).await.unwrap().unwrap();

        assert!(started.unwrap(), "the first run writes it");
        assert!(!again.unwrap(), "a second start changes nothing");
        assert_eq!(
            (pending.status.as_str(), pending.completed_at),
            ("pending", None)
        );
        assert!(
            marked.unwrap(),
            "a pending message is marked before the call"
        );
        assert_eq!((sending.status.as_str(), sending.attempts), ("sending", 1));
        assert!(sending.completed_at.is_none());
        assert!(ended.unwrap(), "the end is written over the sending row");
        assert!(!late.unwrap(), "an ended message never changes");
        assert!(
            !mark_message_sending(&pool, ws, id, 9).await.unwrap(),
            "an ended message is never marked again"
        );
        assert_eq!(
            (
                found.status.as_str(),
                found.reason.as_deref(),
                found.attempts
            ),
            ("failed", Some("render_error"), 2)
        );
        assert!(found.completed_at.is_some());
        assert_eq!(
            found.created_at, pending.created_at,
            "the row keeps its start"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_unconfirmed_note_is_kept_on_a_sending_message_until_it_ends(pool: sqlx::PgPool) {
        use zirv_flow_core::{Slug, WorkspaceName};
        let ws = crate::create_workspace(
            &pool,
            &WorkspaceName::parse("two").unwrap(),
            &Slug::parse("two").unwrap(),
        )
        .await
        .unwrap()
        .id;
        let person = Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id, traits) VALUES ($1, $2, '{}')")
            .bind(person)
            .bind(ws.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        let dimensions = Dimensions::new();
        let id = Uuid::now_v7();
        let message = |status, reason| NewMessage {
            id,
            workspace: ws,
            person,
            enrollment: Uuid::now_v7(),
            flow: None,
            flow_version: None,
            node: "mail",
            template: Uuid::from_u128(1),
            template_version: None,
            destination: Uuid::from_u128(2),
            sender_identity: None,
            category: None,
            topic: None,
            dimensions: &dimensions,
            status,
            reason,
            deciding_rule: None,
            gates: &[],
            attempts: 1,
            now: OffsetDateTime::now_utc(),
        };
        insert_message(&pool, &message(MessageStatus::Pending, None))
            .await
            .unwrap();

        let pending = mark_message_unconfirmed(&pool, ws, id).await.unwrap();
        mark_message_sending(&pool, ws, id, 1).await.unwrap();
        let before = message_unconfirmed(&pool, ws, id).await.unwrap();
        let first = mark_message_unconfirmed(&pool, ws, id).await.unwrap();
        let second = mark_message_unconfirmed(&pool, ws, id).await.unwrap();
        let after = message_unconfirmed(&pool, ws, id).await.unwrap();
        mark_message_sending(&pool, ws, id, 2).await.unwrap();
        let kept = message_unconfirmed(&pool, ws, id).await.unwrap();
        insert_message(
            &pool,
            &message(MessageStatus::Failed, Some("delivery_unconfirmed")),
        )
        .await
        .unwrap();
        let ended = message_unconfirmed(&pool, ws, id).await.unwrap();

        assert!(!pending, "only a sending message takes the note");
        assert!(!before);
        assert!(first && !second, "noted once");
        assert!(after && kept, "a new attempt's mark keeps it");
        assert!(!ended, "an ended message carries no note");
    }

    #[test]
    fn a_gate_record_serializes_codes_and_ids_only() {
        let change = Uuid::from_u128(5);
        let passed = GateRecord::passed("address").to_value();
        let blocked = GateRecord::blocked("consent", "no_consent")
            .with_detail("change", change)
            .to_value();

        assert_eq!(passed, json!({"gate": "address", "result": "passed"}));
        assert_eq!(
            blocked,
            json!({
                "gate": "consent",
                "result": "blocked",
                "reason": "no_consent",
                "change": change,
            })
        );
    }
}
