//! The consent change an unsubscribe link makes (architecture 10.2).

use serde_json::{Map, Value, json};
use sqlx::PgPool;
use time::OffsetDateTime;
use zirv_flow_core::TraceContext;
use zirv_flow_core::consent::{ChangeSource, Channel, ConsentStatus};

use super::TrackedMessage;
use super::engagement::{enrollment_trace, write_event};
use crate::consent::{ConsentChange, ConsentOutcome, dimensions_of, set_consent};
use crate::{StoreError, begin, commit};

/// What [`unsubscribe_message`] did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Unsubscribed {
    /// Consent was withdrawn now, and `email.unsubscribed` was written.
    Changed,
    /// The message's scope was unsubscribed already (or a newer record decides it): nothing new
    /// was written.
    Unchanged,
    /// The person, topic or a dimension value of the message no longer exists.
    Unavailable,
}

/// Denies consent for the message's channel, topic and dimension values with the source
/// `unsubscribe_link`, under the trace the message's enrollment began, and writes the
/// `email.unsubscribed` event. Repeating it writes nothing: [`set_consent`] leaves a status that
/// is held already, so there is no second event or decision.
pub async fn unsubscribe_message(
    pool: &PgPool,
    message: &TrackedMessage,
    now: OffsetDateTime,
    fallback_trace: &TraceContext,
) -> Result<Unsubscribed, StoreError> {
    let mut tx = begin(pool).await?;
    let workspace = message.workspace();
    let trace = enrollment_trace(&mut tx, workspace, message.enrollment_id)
        .await?
        .unwrap_or_else(|| fallback_trace.clone());
    let change = ConsentChange {
        person: message.person_id,
        channel: Channel::Email,
        topic: message.topic_id,
        dimensions: dimensions_of(&message.dimensions),
        status: ConsentStatus::Unsubscribed,
        source: ChangeSource::UnsubscribeLink,
        evidence: json!({"message_id": message.id}),
        occurred_at: now,
        actor: None,
    };
    match set_consent(&mut tx, workspace, &change, &trace).await? {
        ConsentOutcome::Changed { person_id, .. } => {
            let mut properties = Map::new();
            if let Some(topic) = message.topic_id {
                properties.insert("topic_id".to_owned(), Value::from(topic.to_string()));
            }
            write_event(
                &mut tx,
                message,
                person_id,
                "email.unsubscribed",
                properties,
                now,
                &trace,
            )
            .await?;
            commit(tx).await?;
            Ok(Unsubscribed::Changed)
        }
        ConsentOutcome::Unchanged | ConsentOutcome::Superseded => {
            commit(tx).await?;
            Ok(Unsubscribed::Unchanged)
        }
        ConsentOutcome::NoSuchPerson
        | ConsentOutcome::NoSuchTopic
        | ConsentOutcome::NoSuchDimension
        | ConsentOutcome::NoSuchDimensionValue => Ok(Unsubscribed::Unavailable),
    }
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use time::macros::datetime;
    use uuid::Uuid;

    use super::*;
    use crate::flows::tests_support::{trace, workspace};
    use crate::tracking::find_tracked_message;
    use crate::tracking::links::tests::message;

    async fn count(pool: &PgPool, sql: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_unsubscribe_denies_consent_once_and_a_repeat_writes_nothing(pool: PgPool) {
        let ws = workspace(&pool, "unsub").await;
        let (person, id) = message(&pool, ws).await;
        let tracked = find_tracked_message(&pool, id).await.unwrap().unwrap();
        let now = datetime!(2031-05-06 07:08:09 UTC);

        let first = unsubscribe_message(&pool, &tracked, now, &trace())
            .await
            .unwrap();
        let second = unsubscribe_message(&pool, &tracked, now, &trace())
            .await
            .unwrap();

        assert_eq!(first, Unsubscribed::Changed);
        assert_eq!(second, Unsubscribed::Unchanged);
        let status: String = sqlx::query_scalar(
            "SELECT status FROM consent WHERE person_id = $1 AND channel = 'email'",
        )
        .bind(person)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "unsubscribed");
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM events WHERE name = 'email.unsubscribed'"
            )
            .await,
            1
        );
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM consent_changes WHERE source = 'unsubscribe_link'"
            )
            .await,
            1
        );
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM decision_records WHERE action = 'consent.change'"
            )
            .await,
            1
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_message_whose_topic_is_gone_changes_nothing(pool: PgPool) {
        let ws = workspace(&pool, "unsub2").await;
        let (_, id) = message(&pool, ws).await;
        sqlx::query("UPDATE messages SET topic_id = $1 WHERE id = $2")
            .bind(Uuid::now_v7())
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        let tracked = find_tracked_message(&pool, id).await.unwrap().unwrap();

        let result = unsubscribe_message(
            &pool,
            &tracked,
            datetime!(2031-05-06 07:08:09 UTC),
            &trace(),
        )
        .await
        .unwrap();

        assert_eq!(result, Unsubscribed::Unavailable);
        assert_eq!(count(&pool, "SELECT count(*) FROM events").await, 0);
    }
}
