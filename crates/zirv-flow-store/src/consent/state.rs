//! Consent per person, channel and topic, and its append-only history (architecture
//! sections 6 and 10.1).

use serde_json::Value;
use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::consent::{ChangeSource, Channel, ConsentStatus, State};
use zirv_flow_core::segment::ChangeClasses;
use zirv_flow_core::{TraceContext, WorkspaceId};

use super::topics::find_topic;
use crate::StoreError;
use crate::changes::mark_person;
use crate::decisions::{NewDecision, append_decision};
use crate::people::current_person;

/// A change to make: the status a person has for a channel, or for one topic of it.
#[derive(Clone, Debug)]
pub struct ConsentChange {
    /// The person written about; a merged person stands for the survivor.
    pub person: Uuid,
    pub channel: Channel,
    /// `None` is the channel as a whole.
    pub topic: Option<Uuid>,
    pub status: ConsentStatus,
    pub source: ChangeSource,
    /// What proves the decision, such as the form, address or text it came from.
    pub evidence: Value,
    /// When the person decided.
    pub occurred_at: OffsetDateTime,
    pub actor: Option<Uuid>,
}

/// What [`set_consent`] did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsentOutcome {
    /// The status was already that: nothing was written.
    Unchanged,
    Changed {
        /// The person the change landed on: the survivor, when the target was merged.
        person_id: Uuid,
        change_id: Uuid,
    },
    NoSuchPerson,
    /// The topic is not in the workspace, or belongs to another channel.
    NoSuchTopic,
}

/// One current state of a person, with the topic it is about.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct ConsentEntry {
    pub channel: String,
    pub topic_id: Option<Uuid>,
    pub topic_key: Option<String>,
    pub topic_name: Option<String>,
    pub status: String,
    pub explicit: bool,
    pub changed_at: OffsetDateTime,
}

/// One row of a person's consent history.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct ConsentHistoryEntry {
    pub id: Uuid,
    pub channel: String,
    pub topic_id: Option<Uuid>,
    pub topic_key: Option<String>,
    pub from_status: Option<String>,
    pub to_status: String,
    pub source: String,
    pub evidence: Value,
    pub occurred_at: OffsetDateTime,
    pub actor_user_id: Option<Uuid>,
}

/// How many times [`set_consent`] follows a person that is merged away while it looks.
const MERGE_HOPS: usize = 3;

/// Records a consent decision in the caller's transaction. The person is followed through
/// `merged_into`. A write of the status already held writes nothing: no history row and no
/// decision. Otherwise the state is upserted, a history row is appended and a
/// `consent.change` decision records the source code and the ids.
pub async fn set_consent(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    change: &ConsentChange,
    trace: &TraceContext,
) -> Result<ConsentOutcome, StoreError> {
    if let Some(topic) = change.topic {
        match find_topic(&mut *connection, workspace, topic).await? {
            Some(found) if found.channel == change.channel => {}
            _ => return Ok(ConsentOutcome::NoSuchTopic),
        }
    }
    let Some(person) = lock_current_person(connection, workspace, change.person).await? else {
        return Ok(ConsentOutcome::NoSuchPerson);
    };
    let held: Option<String> = sqlx::query_scalar(
        "SELECT status FROM consent WHERE workspace_id = $1 AND person_id = $2 \
           AND channel = $3 AND topic_id IS NOT DISTINCT FROM $4",
    )
    .bind(workspace.as_uuid())
    .bind(person)
    .bind(change.channel.as_str())
    .bind(change.topic)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    if held.as_deref() == Some(change.status.as_str()) {
        return Ok(ConsentOutcome::Unchanged);
    }
    let change_id = write_state(
        connection,
        workspace,
        person,
        &StateWrite {
            channel: change.channel,
            topic: change.topic,
            from: held.as_deref(),
            state: State {
                status: change.status,
                changed_at: change.occurred_at,
            },
            source: change.source,
            evidence: &change.evidence,
            occurred_at: change.occurred_at,
            actor: change.actor,
        },
        trace,
    )
    .await?;
    Ok(ConsentOutcome::Changed {
        person_id: person,
        change_id,
    })
}

/// Locks the person that stands for `person` until the transaction ends, so two writes to
/// one person take turns and a merge cannot move the person's rows underneath this one.
async fn lock_current_person(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
) -> Result<Option<Uuid>, StoreError> {
    let mut target = person;
    for _ in 0..MERGE_HOPS {
        let Some(current) = current_person(&mut *connection, workspace, target).await? else {
            return Ok(None);
        };
        let merged_into: Option<Option<Uuid>> = sqlx::query_scalar(
            "SELECT merged_into FROM people WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
        )
        .bind(workspace.as_uuid())
        .bind(current)
        .fetch_optional(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        match merged_into {
            Some(None) => return Ok(Some(current)),
            Some(Some(survivor)) => target = survivor,
            None => return Ok(None),
        }
    }
    Err(StoreError::retryable_race())
}

/// The state and history row to write for one person, channel and topic.
pub(super) struct StateWrite<'a> {
    pub(super) channel: Channel,
    pub(super) topic: Option<Uuid>,
    pub(super) from: Option<&'a str>,
    pub(super) state: State,
    pub(super) source: ChangeSource,
    pub(super) evidence: &'a Value,
    pub(super) occurred_at: OffsetDateTime,
    pub(super) actor: Option<Uuid>,
}

/// Appends the history row, upserts the state and records the decision. Returns the id of
/// the history row. The caller holds the person's lock.
pub(super) async fn write_state(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
    write: &StateWrite<'_>,
    trace: &TraceContext,
) -> Result<Uuid, StoreError> {
    let change_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO consent_changes \
            (id, workspace_id, person_id, channel, topic_id, from_status, to_status, source, \
             evidence, occurred_at, actor_user_id, trace_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(change_id)
    .bind(workspace.as_uuid())
    .bind(person)
    .bind(write.channel.as_str())
    .bind(write.topic)
    .bind(write.from)
    .bind(write.state.status.as_str())
    .bind(write.source.as_str())
    .bind(write.evidence)
    .bind(write.occurred_at)
    .bind(write.actor)
    .bind(trace.trace_id.as_uuid())
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    sqlx::query(
        "INSERT INTO consent \
            (workspace_id, person_id, channel, topic_id, status, explicit, changed_at, \
             last_change_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         ON CONFLICT (workspace_id, person_id, channel, topic_id) DO UPDATE SET \
            status = EXCLUDED.status, explicit = EXCLUDED.explicit, \
            changed_at = EXCLUDED.changed_at, last_change_id = EXCLUDED.last_change_id",
    )
    .bind(workspace.as_uuid())
    .bind(person)
    .bind(write.channel.as_str())
    .bind(write.topic)
    .bind(write.state.status.as_str())
    .bind(write.state.status.is_explicit())
    .bind(write.state.changed_at)
    .bind(change_id)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    mark_person(
        connection,
        workspace,
        person,
        ChangeClasses::CONSENT,
        trace,
        None,
    )
    .await?;
    append_decision(
        &mut *connection,
        &NewDecision {
            workspace_id: workspace,
            trace: trace.clone(),
            action: "consent.change",
            outcome: "changed",
            reason: Some(write.source.as_str()),
            subject_ids: vec![("person", person), ("consent_change", change_id)],
            job: None,
        },
    )
    .await?;
    Ok(change_id)
}

/// The current states of a person, the channel-wide one first and then by topic key. The
/// person is followed through `merged_into`. `None` when the workspace has no such person.
pub async fn consent_for(
    pool: &PgPool,
    workspace: WorkspaceId,
    person: Uuid,
) -> Result<Option<Vec<ConsentEntry>>, StoreError> {
    let Some(person) = current_person(pool, workspace, person).await? else {
        return Ok(None);
    };
    let entries = sqlx::query_as(
        "SELECT c.channel, c.topic_id, t.key AS topic_key, t.name AS topic_name, c.status, \
                c.explicit, c.changed_at \
         FROM consent c LEFT JOIN topics t ON t.workspace_id = c.workspace_id AND t.id = c.topic_id \
         WHERE c.workspace_id = $1 AND c.person_id = $2 \
         ORDER BY c.channel, t.key NULLS FIRST",
    )
    .bind(workspace.as_uuid())
    .bind(person)
    .fetch_all(pool)
    .await
    .map_err(StoreError::query)?;
    Ok(Some(entries))
}

/// A page of a person's history, newest first by `(occurred_at, id)`, at most `limit`, after
/// the `before` cursor. The flag is true when more existed. The person is followed through
/// `merged_into`; the history of a merged person moved to the survivor. `None` when the
/// workspace has no such person.
pub async fn consent_history(
    pool: &PgPool,
    workspace: WorkspaceId,
    person: Uuid,
    limit: u32,
    before: Option<(OffsetDateTime, Uuid)>,
) -> Result<Option<(Vec<ConsentHistoryEntry>, bool)>, StoreError> {
    let Some(person) = current_person(pool, workspace, person).await? else {
        return Ok(None);
    };
    let mut rows: Vec<ConsentHistoryEntry> = sqlx::query_as(
        "SELECT c.id, c.channel, c.topic_id, t.key AS topic_key, c.from_status, c.to_status,                 c.source, c.evidence, c.occurred_at, c.actor_user_id          FROM consent_changes c          LEFT JOIN topics t ON t.workspace_id = c.workspace_id AND t.id = c.topic_id          WHERE c.workspace_id = $1 AND c.person_id = $2            AND ($3::timestamptz IS NULL OR (c.occurred_at, c.id) < ($3, $4))          ORDER BY c.occurred_at DESC, c.id DESC LIMIT $5",
    )
    .bind(workspace.as_uuid())
    .bind(person)
    .bind(before.map(|(at, _)| at))
    .bind(before.map(|(_, id)| id))
    .bind(i64::from(limit) + 1)
    .fetch_all(pool)
    .await
    .map_err(StoreError::query)?;
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    Ok(Some((rows, more)))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::consent::TopicPolicy;
    use zirv_flow_core::{Cause, Slug, WorkspaceName};

    use super::*;
    use crate::people::{ProfileOutcome, ProfileRecord, apply_profile_batch};
    use crate::{NewTopic, begin, commit, create_topic, create_workspace, list_trace};

    const T1: OffsetDateTime = datetime!(2026-01-01 00:00 UTC);
    const T2: OffsetDateTime = datetime!(2026-01-02 00:00 UTC);

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    async fn person(pool: &PgPool, ws: WorkspaceId, email: &str) -> Uuid {
        let record = ProfileRecord {
            source: "crm".into(),
            stream: "contacts".into(),
            record_key: email.into(),
            identifiers: vec![("email".into(), email.into())],
            traits: serde_json::Map::new(),
            observed_at: datetime!(2026-01-01 00:00 UTC),
            trace: TraceContext::root(Cause::api("t")),
        };
        match apply_profile_batch(pool, ws, &[record])
            .await
            .unwrap()
            .remove(0)
        {
            ProfileOutcome::Applied { person_id, .. }
            | ProfileOutcome::Unchanged { person_id, .. } => person_id,
            ProfileOutcome::Rejected { reason } => panic!("rejected: {reason}"),
        }
    }

    async fn topic(pool: &PgPool, ws: WorkspaceId, key: &str) -> Uuid {
        create_topic(
            pool,
            ws,
            &NewTopic {
                key: key.into(),
                name: key.into(),
                description: String::new(),
                channel: Channel::Email,
                policy: TopicPolicy::OptOut,
            },
        )
        .await
        .unwrap()
        .id
    }

    fn change(
        person: Uuid,
        topic: Option<Uuid>,
        status: ConsentStatus,
        at: OffsetDateTime,
    ) -> ConsentChange {
        ConsentChange {
            person,
            channel: Channel::Email,
            topic,
            status,
            source: ChangeSource::Api,
            evidence: json!({"form": "footer"}),
            occurred_at: at,
            actor: None,
        }
    }

    async fn set(
        pool: &PgPool,
        ws: WorkspaceId,
        change: &ConsentChange,
        trace: &TraceContext,
    ) -> ConsentOutcome {
        let mut tx = begin(pool).await.unwrap();
        let outcome = set_consent(&mut tx, ws, change, trace).await.unwrap();
        commit(tx).await.unwrap();
        outcome
    }

    async fn count(pool: &PgPool, table: &'static str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_change_is_stored_with_history_and_a_decision_that_holds_ids_only(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let ada = person(&pool, ws, "ada@example.com").await;
        let news = topic(&pool, ws, "news").await;
        let trace = TraceContext::root(Cause::api("test"));

        let outcome = set(
            &pool,
            ws,
            &change(ada, Some(news), ConsentStatus::Subscribed, T1),
            &trace,
        )
        .await;

        let ConsentOutcome::Changed {
            person_id,
            change_id,
        } = outcome
        else {
            panic!("{outcome:?}");
        };
        assert_eq!(person_id, ada);
        let entries = consent_for(&pool, ws, ada).await.unwrap().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].topic_key.as_deref(), Some("news"));
        assert_eq!(entries[0].status, "subscribed");
        assert!(entries[0].explicit);
        assert_eq!(entries[0].changed_at, T1);
        let (history, more) = consent_history(&pool, ws, ada, 10, None)
            .await
            .unwrap()
            .unwrap();
        assert!(!more);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, change_id);
        assert_eq!(history[0].from_status, None);
        assert_eq!(history[0].to_status, "subscribed");
        assert_eq!(history[0].source, "api");
        assert_eq!(history[0].evidence, json!({"form": "footer"}));

        let (decisions, _) = list_trace(&pool, ws, trace.trace_id, 20).await.unwrap();
        let decision = decisions
            .iter()
            .find(|d| d.action == "consent.change")
            .unwrap();
        assert_eq!(decision.reason.as_deref(), Some("api"));
        assert_eq!(
            decision.subject_ids,
            json!({"person": ada.to_string(), "consent_change": change_id.to_string()})
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn writing_the_status_already_held_leaves_no_history_and_no_decision(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let ada = person(&pool, ws, "ada@example.com").await;
        let trace = TraceContext::root(Cause::api("test"));
        set(
            &pool,
            ws,
            &change(ada, None, ConsentStatus::Subscribed, T1),
            &trace,
        )
        .await;
        let decisions_before = count(&pool, "decision_records").await;

        let outcome = set(
            &pool,
            ws,
            &change(ada, None, ConsentStatus::Subscribed, T2),
            &trace,
        )
        .await;

        assert_eq!(outcome, ConsentOutcome::Unchanged);
        assert_eq!(count(&pool, "consent_changes").await, 1);
        assert_eq!(count(&pool, "decision_records").await, decisions_before);
        let changed_at: OffsetDateTime = sqlx::query_scalar("SELECT changed_at FROM consent")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(changed_at, T1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_second_change_records_where_it_came_from_and_the_history_pages_newest_first(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let ada = person(&pool, ws, "ada@example.com").await;
        let trace = TraceContext::root(Cause::api("test"));
        set(
            &pool,
            ws,
            &change(ada, None, ConsentStatus::Subscribed, T1),
            &trace,
        )
        .await;
        let mut second = change(ada, None, ConsentStatus::Unsubscribed, T2);
        second.source = ChangeSource::UnsubscribeLink;
        set(&pool, ws, &second, &trace).await;

        let (page, more) = consent_history(&pool, ws, ada, 1, None)
            .await
            .unwrap()
            .unwrap();

        assert!(more);
        assert_eq!(page[0].to_status, "unsubscribed");
        assert_eq!(page[0].from_status.as_deref(), Some("subscribed"));
        assert_eq!(page[0].source, "unsubscribe_link");
        let (rest, more) =
            consent_history(&pool, ws, ada, 1, Some((page[0].occurred_at, page[0].id)))
                .await
                .unwrap()
                .unwrap();
        assert!(!more);
        assert_eq!(rest[0].to_status, "subscribed");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_channel_wide_state_and_a_topic_state_are_separate_rows(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let ada = person(&pool, ws, "ada@example.com").await;
        let news = topic(&pool, ws, "news").await;
        let trace = TraceContext::root(Cause::api("test"));

        set(
            &pool,
            ws,
            &change(ada, None, ConsentStatus::Unsubscribed, T1),
            &trace,
        )
        .await;
        set(
            &pool,
            ws,
            &change(ada, Some(news), ConsentStatus::Subscribed, T1),
            &trace,
        )
        .await;

        let entries = consent_for(&pool, ws, ada).await.unwrap().unwrap();
        let shown: Vec<(Option<&str>, &str)> = entries
            .iter()
            .map(|entry| (entry.topic_key.as_deref(), entry.status.as_str()))
            .collect();
        assert_eq!(
            shown,
            [(None, "unsubscribed"), (Some("news"), "subscribed")]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_topic_of_another_workspace_or_an_unknown_person_changes_nothing(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let ada = person(&pool, one, "ada@example.com").await;
        let foreign_topic = topic(&pool, two, "news").await;
        let trace = TraceContext::root(Cause::api("test"));

        let wrong_topic = set(
            &pool,
            one,
            &change(ada, Some(foreign_topic), ConsentStatus::Subscribed, T1),
            &trace,
        )
        .await;
        let wrong_person = set(
            &pool,
            two,
            &change(ada, None, ConsentStatus::Subscribed, T1),
            &trace,
        )
        .await;
        let nobody = set(
            &pool,
            one,
            &change(Uuid::now_v7(), None, ConsentStatus::Subscribed, T1),
            &trace,
        )
        .await;

        assert_eq!(wrong_topic, ConsentOutcome::NoSuchTopic);
        assert_eq!(wrong_person, ConsentOutcome::NoSuchPerson);
        assert_eq!(nobody, ConsentOutcome::NoSuchPerson);
        assert_eq!(count(&pool, "consent").await, 0);
        assert!(consent_for(&pool, two, ada).await.unwrap().is_none());
        assert!(
            consent_history(&pool, two, ada, 5, None)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_write_for_a_merged_person_lands_on_the_survivor(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let survivor = person(&pool, ws, "ada@example.com").await;
        let merged = person(&pool, ws, "ada2@example.com").await;
        sqlx::query("UPDATE people SET merged_into = $1 WHERE id = $2")
            .bind(survivor)
            .bind(merged)
            .execute(&pool)
            .await
            .unwrap();
        let trace = TraceContext::root(Cause::api("test"));

        let outcome = set(
            &pool,
            ws,
            &change(merged, None, ConsentStatus::Unsubscribed, T1),
            &trace,
        )
        .await;

        assert!(
            matches!(outcome, ConsentOutcome::Changed { person_id, .. } if person_id == survivor)
        );
        let through_merged = consent_for(&pool, ws, merged).await.unwrap().unwrap();
        assert_eq!(through_merged.len(), 1);
        let owner: Uuid = sqlx::query_scalar("SELECT person_id FROM consent")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(owner, survivor);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_history_cannot_be_updated_but_can_be_deleted_for_erasure(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let ada = person(&pool, ws, "ada@example.com").await;
        let trace = TraceContext::root(Cause::api("test"));
        set(
            &pool,
            ws,
            &change(ada, None, ConsentStatus::Subscribed, T1),
            &trace,
        )
        .await;

        let rewrite = sqlx::query("UPDATE consent_changes SET to_status = 'unsubscribed'")
            .execute(&pool)
            .await;
        let evidence = sqlx::query("UPDATE consent_changes SET evidence = '{}'")
            .execute(&pool)
            .await;
        let repoint = sqlx::query("UPDATE consent_changes SET person_id = person_id")
            .execute(&pool)
            .await;
        let stranger = person(&pool, ws, "stranger@example.com").await;
        let to_stranger = sqlx::query("UPDATE consent_changes SET person_id = $1")
            .bind(stranger)
            .execute(&pool)
            .await;
        sqlx::query("DELETE FROM consent_changes")
            .execute(&pool)
            .await
            .unwrap();

        assert!(rewrite.unwrap_err().to_string().contains("append-only"));
        assert!(evidence.is_err());
        assert!(repoint.is_ok());
        assert!(to_stranger.unwrap_err().to_string().contains("survivor"));
        assert_eq!(count(&pool, "consent_changes").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn history_moves_only_to_the_survivor_of_a_merge(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let loser = person(&pool, ws, "ada@example.com").await;
        let survivor = person(&pool, ws, "ada2@example.com").await;
        let stranger = person(&pool, ws, "bob@example.com").await;
        let trace = TraceContext::root(Cause::api("test"));
        set(
            &pool,
            ws,
            &change(loser, None, ConsentStatus::Subscribed, T1),
            &trace,
        )
        .await;
        sqlx::query("UPDATE people SET merged_into = $1 WHERE id = $2")
            .bind(survivor)
            .bind(loser)
            .execute(&pool)
            .await
            .unwrap();

        let to_stranger = sqlx::query("UPDATE consent_changes SET person_id = $1")
            .bind(stranger)
            .execute(&pool)
            .await;
        let to_survivor = sqlx::query("UPDATE consent_changes SET person_id = $1")
            .bind(survivor)
            .execute(&pool)
            .await;

        assert!(to_stranger.is_err());
        assert_eq!(to_survivor.unwrap().rows_affected(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn evidence_that_is_not_an_object_is_refused_by_the_database(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let ada = person(&pool, ws, "ada@example.com").await;
        let trace = TraceContext::root(Cause::api("test"));
        let mut tx = begin(&pool).await.unwrap();
        let mut array = change(ada, None, ConsentStatus::Subscribed, T1);
        array.evidence = json!(["x"]);

        let not_object = set_consent(&mut tx, ws, &array, &trace).await;

        assert!(not_object.is_err());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_consent_change_marks_the_person_in_the_consent_class_and_a_repeat_marks_nobody(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let ada = person(&pool, ws, "ada@example.com").await;
        let trace = TraceContext::root(Cause::api("consent"));
        let change = change(ada, None, ConsentStatus::Subscribed, T1);
        sqlx::query("DELETE FROM person_changes")
            .execute(&pool)
            .await
            .unwrap();

        set(&pool, ws, &change, &trace).await;

        let marks: Vec<(Uuid, i16, Option<Uuid>, Option<String>)> =
            sqlx::query_as("SELECT person_id, classes, trace_id, cause_id FROM person_changes")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            marks,
            [(
                ada,
                8,
                Some(trace.trace_id.as_uuid()),
                Some("consent".into())
            )]
        );
        sqlx::query("DELETE FROM person_changes")
            .execute(&pool)
            .await
            .unwrap();

        set(&pool, ws, &change, &trace).await;

        assert_eq!(count(&pool, "person_changes").await, 0);
    }
}
