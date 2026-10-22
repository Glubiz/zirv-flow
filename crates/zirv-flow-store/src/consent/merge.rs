//! Consent at merge (architecture section 7.3): the loser's states are combined with the
//! survivor's by the pure rule in `zirv_flow_core::consent`, and its history moves along.

use serde_json::{Value, json};
use sqlx::PgConnection;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::consent::{ChangeSource, Channel, ConsentStatus, Record, State, merge_records};
use zirv_flow_core::{TraceContext, WorkspaceId};

use super::state::{StateWrite, dimensions_of, write_state};
use crate::StoreError;

#[derive(FromRow)]
struct StateRow {
    channel: String,
    topic_id: Option<Uuid>,
    dimensions: Value,
    status: String,
    changed_at: OffsetDateTime,
    last_change_id: Option<Uuid>,
}

impl StateRow {
    fn state(&self) -> Result<State, StoreError> {
        let status: ConsentStatus = self
            .status
            .parse()
            .map_err(|_| StoreError::query(sqlx::Error::Decode("unknown consent status".into())))?;
        Ok(State {
            status,
            changed_at: self.changed_at,
        })
    }

    /// The row as a record of the pure rule. The topic is the same on both sides of a
    /// comparison, so the scope holds the workspace dimensions only.
    fn record(&self) -> Result<Record, StoreError> {
        Ok(Record {
            scope: dimensions_of(&self.dimensions),
            state: self.state()?,
            change_id: self.last_change_id.unwrap_or_else(Uuid::nil),
        })
    }
}

/// Moves what `loser` holds to `survivor`, in the caller's transaction, which holds the locks
/// of both people. For each channel, topic and dimension scope the loser has a state for, the
/// survivor ends up with the rule's winner (`merge_records`). A change of status is recorded
/// in the history with source `merge` and the merged person's id as evidence. The loser's
/// states are deleted and its history is re-pointed to the survivor.
pub(crate) async fn move_on_merge(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    loser: Uuid,
    survivor: Uuid,
    trace: &TraceContext,
) -> Result<(), StoreError> {
    let losing: Vec<StateRow> = sqlx::query_as(
        "SELECT channel, topic_id, dimensions, status, changed_at, last_change_id FROM consent \
         WHERE workspace_id = $1 AND person_id = $2 \
         ORDER BY channel, topic_id NULLS FIRST, dimensions_key",
    )
    .bind(workspace.as_uuid())
    .bind(loser)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let evidence = json!({ "merged_person_id": loser });
    for row in &losing {
        let channel: Channel = row
            .channel
            .parse()
            .map_err(|_| StoreError::query(sqlx::Error::Decode("unknown channel".into())))?;
        let held: Option<StateRow> = sqlx::query_as(
            "SELECT channel, topic_id, dimensions, status, changed_at, last_change_id \
             FROM consent \
             WHERE workspace_id = $1 AND person_id = $2 AND channel = $3 \
               AND topic_id IS NOT DISTINCT FROM $4 \
               AND dimensions_key = consent_dimensions_canonical($5) FOR UPDATE",
        )
        .bind(workspace.as_uuid())
        .bind(survivor)
        .bind(&row.channel)
        .bind(row.topic_id)
        .bind(&row.dimensions)
        .fetch_optional(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        let held_state = held.as_ref().map(StateRow::state).transpose()?;
        let held_record = held.as_ref().map(StateRow::record).transpose()?;
        let Some(winner) = merge_records(held_record.into_iter().collect(), vec![row.record()?])
            .pop()
            .map(|record| record.state)
        else {
            continue;
        };
        if held_state.map(|state| state.status) != Some(winner.status) {
            write_state(
                connection,
                workspace,
                survivor,
                &StateWrite {
                    channel,
                    topic: row.topic_id,
                    dimensions: &dimensions_of(&row.dimensions),
                    from: held.as_ref().map(|held| held.status.as_str()),
                    state: winner,
                    source: ChangeSource::Merge,
                    evidence: &evidence,
                    occurred_at: OffsetDateTime::now_utc(),
                    actor: None,
                },
                trace,
            )
            .await?;
        } else if held_state != Some(winner) {
            sqlx::query(
                "UPDATE consent SET changed_at = $5 \
                 WHERE workspace_id = $1 AND person_id = $2 AND channel = $3 \
                   AND topic_id IS NOT DISTINCT FROM $4 \
                   AND dimensions_key = consent_dimensions_canonical($6)",
            )
            .bind(workspace.as_uuid())
            .bind(survivor)
            .bind(&row.channel)
            .bind(row.topic_id)
            .bind(winner.changed_at)
            .bind(&row.dimensions)
            .execute(&mut *connection)
            .await
            .map_err(StoreError::query)?;
        }
    }
    sqlx::query("DELETE FROM consent WHERE workspace_id = $1 AND person_id = $2")
        .bind(workspace.as_uuid())
        .bind(loser)
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    sqlx::query(
        "UPDATE consent_changes SET person_id = $3 WHERE workspace_id = $1 AND person_id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(loser)
    .bind(survivor)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::consent::TopicPolicy;
    use zirv_flow_core::{Cause, Slug, WorkspaceName};

    use super::*;
    use crate::consent::{
        ConsentChange, NewTopic, consent_for, consent_history, create_topic, set_consent,
    };
    use crate::people::{ProfileOutcome, ProfileRecord, apply_profile_batch};
    use crate::{begin, commit, create_workspace};

    const T1: OffsetDateTime = datetime!(2026-01-01 00:00 UTC);
    const T2: OffsetDateTime = datetime!(2026-01-02 00:00 UTC);

    async fn workspace(pool: &PgPool) -> WorkspaceId {
        let name = WorkspaceName::parse("one").unwrap();
        let slug = Slug::parse("one").unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    fn record(key: &str, identifiers: &[(&str, &str)]) -> ProfileRecord {
        ProfileRecord {
            source: "crm".into(),
            stream: "contacts".into(),
            record_key: key.into(),
            identifiers: identifiers
                .iter()
                .map(|(kind, value)| ((*kind).into(), (*value).into()))
                .collect(),
            traits: serde_json::Map::new(),
            observed_at: datetime!(2026-01-01 00:00 UTC),
            trace: TraceContext::root(Cause::api("t")),
        }
    }

    async fn apply(pool: &PgPool, ws: WorkspaceId, record: ProfileRecord) -> Uuid {
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

    async fn topic(pool: &PgPool, ws: WorkspaceId) -> Uuid {
        create_topic(
            pool,
            ws,
            &NewTopic {
                key: "newsletter".into(),
                name: "Newsletter".into(),
                description: String::new(),
                channel: Channel::Email,
                policy: TopicPolicy::OptOut,
            },
        )
        .await
        .unwrap()
        .id
    }

    async fn set(
        pool: &PgPool,
        ws: WorkspaceId,
        person: Uuid,
        topic: Option<Uuid>,
        status: ConsentStatus,
        at: OffsetDateTime,
    ) {
        let mut tx = begin(pool).await.unwrap();
        set_consent(
            &mut tx,
            ws,
            &ConsentChange {
                dimensions: Default::default(),
                person,
                channel: Channel::Email,
                topic,
                status,
                source: ChangeSource::Api,
                evidence: json!({}),
                occurred_at: at,
                actor: None,
            },
            &TraceContext::root(Cause::api("t")),
        )
        .await
        .unwrap();
        commit(tx).await.unwrap();
    }

    /// Two people that share an anonymous id once a third record names both.
    async fn two_people(pool: &PgPool, ws: WorkspaceId) -> (Uuid, Uuid) {
        let survivor = apply(pool, ws, record("1", &[("external:crm", "1")])).await;
        let loser = apply(pool, ws, record("2", &[("anonymous_id", "a")])).await;
        (survivor, loser)
    }

    async fn join(pool: &PgPool, ws: WorkspaceId) -> Uuid {
        apply(
            pool,
            ws,
            record("3", &[("external:crm", "1"), ("anonymous_id", "a")]),
        )
        .await
    }

    async fn states(
        pool: &PgPool,
        ws: WorkspaceId,
        person: Uuid,
    ) -> Vec<(Option<String>, String, OffsetDateTime)> {
        consent_for(pool, ws, person)
            .await
            .unwrap()
            .unwrap()
            .into_iter()
            .map(|entry| (entry.topic_key, entry.status, entry.changed_at))
            .collect()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn unsubscribed_wins_a_tie_and_the_merge_is_in_the_survivors_history(pool: PgPool) {
        let ws = workspace(&pool).await;
        let newsletter = topic(&pool, ws).await;
        let (p1, p2) = two_people(&pool, ws).await;
        set(
            &pool,
            ws,
            p1,
            Some(newsletter),
            ConsentStatus::Subscribed,
            T1,
        )
        .await;
        set(
            &pool,
            ws,
            p2,
            Some(newsletter),
            ConsentStatus::Unsubscribed,
            T1,
        )
        .await;

        let survivor = join(&pool, ws).await;

        assert_eq!(survivor, p1);
        assert_eq!(
            states(&pool, ws, p1).await,
            [(Some("newsletter".to_owned()), "unsubscribed".to_owned(), T1)]
        );
        let (history, _) = consent_history(&pool, ws, p1, 10, None, &Default::default())
            .await
            .unwrap()
            .unwrap();
        let sources: Vec<(&str, &str)> = history
            .iter()
            .map(|entry| (entry.source.as_str(), entry.to_status.as_str()))
            .collect();
        assert_eq!(
            sources,
            [
                ("merge", "unsubscribed"),
                ("api", "unsubscribed"),
                ("api", "subscribed")
            ]
        );
        assert_eq!(history[0].from_status.as_deref(), Some("subscribed"));
        assert_eq!(history[0].evidence, json!({"merged_person_id": p2}));
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM consent WHERE person_id = $1")
            .bind(p2)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 0);
        let changes_left: i64 =
            sqlx::query_scalar("SELECT count(*) FROM consent_changes WHERE person_id = $1")
                .bind(p2)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(changes_left, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_later_decision_wins_and_states_only_the_loser_holds_move_over(pool: PgPool) {
        let ws = workspace(&pool).await;
        let newsletter = topic(&pool, ws).await;
        let (p1, p2) = two_people(&pool, ws).await;
        set(
            &pool,
            ws,
            p1,
            Some(newsletter),
            ConsentStatus::Unsubscribed,
            T1,
        )
        .await;
        set(
            &pool,
            ws,
            p2,
            Some(newsletter),
            ConsentStatus::Subscribed,
            T2,
        )
        .await;
        set(&pool, ws, p2, None, ConsentStatus::PendingConfirmation, T1).await;

        join(&pool, ws).await;

        assert_eq!(
            states(&pool, ws, p1).await,
            [
                (None, "pending_confirmation".to_owned(), T1),
                (Some("newsletter".to_owned()), "subscribed".to_owned(), T2),
            ]
        );
        let merge_rows: Vec<(Option<String>, String)> = sqlx::query_as(
            "SELECT from_status, to_status FROM consent_changes WHERE source = 'merge' \
             ORDER BY to_status",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            merge_rows,
            [
                (None, "pending_confirmation".to_owned()),
                (Some("unsubscribed".to_owned()), "subscribed".to_owned()),
            ]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_merge_keeps_the_latest_change_per_dimension_scope(pool: PgPool) {
        use crate::{NewDimension, create_dimension, create_dimension_value};
        let ws = workspace(&pool).await;
        create_dimension(
            &pool,
            ws,
            &NewDimension {
                key: "vertical".into(),
                name: "Vertical".into(),
            },
        )
        .await
        .unwrap();
        for key in ["retail", "b2b"] {
            create_dimension_value(&pool, ws, "vertical", key, key)
                .await
                .unwrap();
        }
        let (p1, p2) = two_people(&pool, ws).await;
        let scoped = async |person: Uuid, vertical: Option<&str>, status, at| {
            let mut tx = begin(&pool).await.unwrap();
            set_consent(
                &mut tx,
                ws,
                &ConsentChange {
                    person,
                    channel: Channel::Email,
                    topic: None,
                    dimensions: vertical
                        .map(|value| [("vertical".to_owned(), value.to_owned())].into())
                        .unwrap_or_default(),
                    status,
                    source: ChangeSource::Api,
                    evidence: json!({}),
                    occurred_at: at,
                    actor: None,
                },
                &TraceContext::root(Cause::api("t")),
            )
            .await
            .unwrap();
            commit(tx).await.unwrap();
        };
        scoped(p1, None, ConsentStatus::Subscribed, T1).await;
        scoped(p1, Some("retail"), ConsentStatus::Subscribed, T2).await;
        scoped(p2, None, ConsentStatus::Unsubscribed, T2).await;
        scoped(p2, Some("retail"), ConsentStatus::Unsubscribed, T1).await;
        scoped(p2, Some("b2b"), ConsentStatus::Unsubscribed, T1).await;

        join(&pool, ws).await;

        let rows: Vec<(String, String, OffsetDateTime)> = sqlx::query_as(
            "SELECT dimensions_key, status, changed_at FROM consent WHERE person_id = $1 \
             ORDER BY dimensions_key",
        )
        .bind(p1)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            [
                (String::new(), "unsubscribed".to_owned(), T2),
                ("vertical=b2b".to_owned(), "unsubscribed".to_owned(), T1),
                ("vertical=retail".to_owned(), "subscribed".to_owned(), T2),
            ]
        );
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM consent WHERE person_id = $1")
            .bind(p2)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 0);
        let merged: Vec<(Value, String)> = sqlx::query_as(
            "SELECT dimensions, to_status FROM consent_changes WHERE source = 'merge' \
             ORDER BY dimensions::text",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            merged,
            [
                (json!({}), "unsubscribed".to_owned()),
                (json!({"vertical": "b2b"}), "unsubscribed".to_owned()),
            ]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_survivor_that_already_wins_gets_no_history_row(pool: PgPool) {
        let ws = workspace(&pool).await;
        let (p1, p2) = two_people(&pool, ws).await;
        set(&pool, ws, p1, None, ConsentStatus::Unsubscribed, T2).await;
        set(&pool, ws, p2, None, ConsentStatus::Subscribed, T1).await;

        join(&pool, ws).await;

        assert_eq!(
            states(&pool, ws, p1).await,
            [(None, "unsubscribed".to_owned(), T2)]
        );
        let merges: i64 =
            sqlx::query_scalar("SELECT count(*) FROM consent_changes WHERE source = 'merge'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(merges, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_later_decision_of_the_same_status_moves_the_time_without_history(pool: PgPool) {
        let ws = workspace(&pool).await;
        let (p1, p2) = two_people(&pool, ws).await;
        set(&pool, ws, p1, None, ConsentStatus::Subscribed, T1).await;
        set(&pool, ws, p2, None, ConsentStatus::Subscribed, T2).await;

        join(&pool, ws).await;

        assert_eq!(
            states(&pool, ws, p1).await,
            [(None, "subscribed".to_owned(), T2)]
        );
        let all: Vec<Value> = sqlx::query_scalar("SELECT to_jsonb(source) FROM consent_changes")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert!(all.iter().all(|source| source != "merge"));
    }
}
