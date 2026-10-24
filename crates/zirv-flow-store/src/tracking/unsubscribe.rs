//! The consent change an unsubscribe link makes (architecture 10.2).

use serde_json::{Map, Value, json};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::TraceContext;
use zirv_flow_core::consent::{ChangeSource, Channel, ConsentStatus, Dimensions};

use super::TrackedMessage;
use super::engagement::{message_trace, write_event};
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
    /// The person of the message no longer exists.
    Unavailable,
}

/// The properties of an `email.unsubscribed` event for a scope: the topic id and the dimension
/// keys with their value keys. Ids and keys only, never an address.
fn scope_properties(topic: Option<Uuid>, dimensions: &Dimensions) -> Map<String, Value> {
    let mut properties = Map::new();
    if let Some(topic) = topic {
        properties.insert("topic_id".to_owned(), Value::from(topic.to_string()));
    }
    if !dimensions.is_empty() {
        let scope: Map<String, Value> = dimensions
            .iter()
            .map(|(key, value)| (key.clone(), Value::from(value.clone())))
            .collect();
        properties.insert("dimensions".to_owned(), Value::Object(scope));
    }
    properties
}

/// Denies consent for the message's channel, topic and dimension values with the source
/// `unsubscribe_link`, under the trace the message's enrollment began, and writes the
/// `email.unsubscribed` event. Repeating it writes nothing: [`set_consent`] leaves a status that
/// is held already, so there is no second event or decision.
///
/// An unsubscribe always works. When the message's topic or a dimension value no longer exists,
/// the broadest scope that still exists is denied instead: the message's channel with no topic
/// and no dimension values. The change's evidence then says `"fallback": "channel"`. Only a
/// person who is gone makes the result [`Unsubscribed::Unavailable`].
pub async fn unsubscribe_message(
    pool: &PgPool,
    message: &TrackedMessage,
    now: OffsetDateTime,
    fallback_trace: &TraceContext,
) -> Result<Unsubscribed, StoreError> {
    let mut tx = begin(pool).await?;
    let workspace = message.workspace();
    let trace = message_trace(&mut tx, message, fallback_trace).await?;
    let scoped = (message.topic_id, dimensions_of(&message.dimensions));
    let broadest = (None, Dimensions::new());
    let mut attempts = vec![(scoped.clone(), false)];
    if scoped != broadest {
        attempts.push((broadest, true));
    }
    for ((topic, dimensions), fell_back) in attempts {
        let evidence = if fell_back {
            json!({"message_id": message.id, "fallback": "channel"})
        } else {
            json!({"message_id": message.id})
        };
        let change = ConsentChange {
            person: message.person_id,
            channel: Channel::Email,
            topic,
            dimensions: dimensions.clone(),
            status: ConsentStatus::Unsubscribed,
            source: ChangeSource::UnsubscribeLink,
            evidence,
            occurred_at: now,
            actor: None,
        };
        match set_consent(&mut tx, workspace, &change, &trace).await? {
            ConsentOutcome::Changed { person_id, .. } => {
                write_event(
                    &mut tx,
                    message,
                    person_id,
                    "email.unsubscribed",
                    scope_properties(topic, &dimensions),
                    now,
                    &trace,
                )
                .await?;
                commit(tx).await?;
                return Ok(Unsubscribed::Changed);
            }
            ConsentOutcome::Unchanged | ConsentOutcome::Superseded => {
                commit(tx).await?;
                return Ok(Unsubscribed::Unchanged);
            }
            ConsentOutcome::NoSuchPerson => break,
            ConsentOutcome::NoSuchTopic
            | ConsentOutcome::NoSuchDimension
            | ConsentOutcome::NoSuchDimensionValue => {}
        }
    }
    Ok(Unsubscribed::Unavailable)
}

/// One choice a person made on the preference page: subscribe to or leave a scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreferenceChange {
    /// The topic; none for the channel as a whole.
    pub topic: Option<Uuid>,
    /// The dimension values the choice is scoped to; a dimension left out means all its values.
    pub dimensions: Dimensions,
    pub subscribed: bool,
}

/// What [`save_preferences`] did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PreferencesSaved {
    /// How many choices changed what is recorded.
    pub changed: usize,
    /// Whether a choice named a topic or dimension value that no longer exists, or the person is
    /// gone. Nothing is written in that case.
    pub unavailable: bool,
}

/// Records the choices of the preference page with the source `unsubscribe_link`, in one
/// transaction under the trace the message's enrollment began. A choice that repeats what is held
/// changes nothing. If any choice cannot be recorded, none is. Each choice that changed to
/// denied also writes an `email.unsubscribed` event, as the unsubscribe route does, with its
/// scope; an opt-in writes no event.
pub async fn save_preferences(
    pool: &PgPool,
    message: &TrackedMessage,
    choices: &[PreferenceChange],
    now: OffsetDateTime,
    fallback_trace: &TraceContext,
) -> Result<PreferencesSaved, StoreError> {
    let mut tx = begin(pool).await?;
    let workspace = message.workspace();
    let trace = message_trace(&mut tx, message, fallback_trace).await?;
    let mut changed = 0;
    for choice in choices {
        let change = ConsentChange {
            person: message.person_id,
            channel: Channel::Email,
            topic: choice.topic,
            dimensions: choice.dimensions.clone(),
            status: if choice.subscribed {
                ConsentStatus::Subscribed
            } else {
                ConsentStatus::Unsubscribed
            },
            source: ChangeSource::UnsubscribeLink,
            evidence: json!({"message_id": message.id}),
            occurred_at: now,
            actor: None,
        };
        match set_consent(&mut tx, workspace, &change, &trace).await? {
            ConsentOutcome::Changed { person_id, .. } => {
                changed += 1;
                if !choice.subscribed {
                    write_event(
                        &mut tx,
                        message,
                        person_id,
                        "email.unsubscribed",
                        scope_properties(choice.topic, &choice.dimensions),
                        now,
                        &trace,
                    )
                    .await?;
                }
            }
            ConsentOutcome::Unchanged | ConsentOutcome::Superseded => {}
            ConsentOutcome::NoSuchPerson
            | ConsentOutcome::NoSuchTopic
            | ConsentOutcome::NoSuchDimension
            | ConsentOutcome::NoSuchDimensionValue => {
                return Ok(PreferencesSaved {
                    changed: 0,
                    unavailable: true,
                });
            }
        }
    }
    commit(tx).await?;
    Ok(PreferencesSaved {
        changed,
        unavailable: false,
    })
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
    async fn preference_choices_are_recorded_together_and_a_repeat_changes_nothing(pool: PgPool) {
        let ws = workspace(&pool, "prefs").await;
        let (_, id) = message(&pool, ws).await;
        let tracked = find_tracked_message(&pool, id).await.unwrap().unwrap();
        let now = datetime!(2031-05-06 07:08:09 UTC);
        let choices = [PreferenceChange {
            topic: None,
            dimensions: Dimensions::new(),
            subscribed: false,
        }];

        let first = save_preferences(&pool, &tracked, &choices, now, &trace())
            .await
            .unwrap();
        let again = save_preferences(&pool, &tracked, &choices, now, &trace())
            .await
            .unwrap();

        assert_eq!((first.changed, first.unavailable), (1, false));
        assert_eq!((again.changed, again.unavailable), (0, false));
        let bad = [
            choices[0].clone(),
            PreferenceChange {
                topic: Some(Uuid::now_v7()),
                dimensions: Dimensions::new(),
                subscribed: true,
            },
        ];
        let refused = save_preferences(&pool, &tracked, &bad, now, &trace())
            .await
            .unwrap();
        assert!(refused.unavailable);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_save_writes_one_event_per_opt_out_and_none_per_opt_in(pool: PgPool) {
        let ws = workspace(&pool, "prefs-events").await;
        let (_, id) = message(&pool, ws).await;
        let tracked = find_tracked_message(&pool, id).await.unwrap().unwrap();
        let topic = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO topics (id, workspace_id, key, name, channel, policy) \
             VALUES ($1, $2, 'news', 'News', 'email', 'opt_in')",
        )
        .bind(topic)
        .bind(ws.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
        let offers = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO topics (id, workspace_id, key, name, channel, policy)              VALUES ($1, $2, 'offers', 'Offers', 'email', 'opt_in')",
        )
        .bind(offers)
        .bind(ws.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
        let dimension = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO consent_dimensions (id, workspace_id, key, name)              VALUES ($1, $2, 'region', 'Region')",
        )
        .bind(dimension)
        .bind(ws.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO consent_dimension_values (id, workspace_id, dimension_id, key, name)              VALUES ($1, $2, $3, 'north', 'North')",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .bind(dimension)
        .execute(&pool)
        .await
        .unwrap();
        let choices = [
            PreferenceChange {
                topic: Some(topic),
                dimensions: Dimensions::new(),
                subscribed: true,
            },
            PreferenceChange {
                topic: Some(offers),
                dimensions: Dimensions::from([("region".to_owned(), "north".to_owned())]),
                subscribed: false,
            },
        ];

        let saved = save_preferences(
            &pool,
            &tracked,
            &choices,
            datetime!(2031-05-06 07:08:09 UTC),
            &trace(),
        )
        .await
        .unwrap();

        assert_eq!((saved.changed, saved.unavailable), (2, false));
        let events: Vec<Value> =
            sqlx::query_scalar("SELECT properties FROM events WHERE name = 'email.unsubscribed'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0]["topic_id"], json!(offers.to_string()));
        assert_eq!(events[0]["dimensions"], json!({"region": "north"}));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_message_whose_topic_is_gone_falls_back_to_the_channel(pool: PgPool) {
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

        assert_eq!(result, Unsubscribed::Changed);
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
                "SELECT count(*) FROM consent_changes WHERE evidence->>'fallback' = 'channel'"
            )
            .await,
            1
        );
    }
}
