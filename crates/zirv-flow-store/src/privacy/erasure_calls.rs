//! The erase calls of an erasure (architecture 16.3, "As built (erase calls, #227)").
//!
//! The store never decides which destinations erase: it depends on core only and cannot read a
//! manifest. The caller lists the targets (a destination and where its external key comes from),
//! and [`plan`], inside the erasure transaction and before the delete, keeps the targets that
//! handled the person, resolves each external key from the person's rows and enqueues one
//! `destination.erase` job per kept target. The job's payload holds the key and no trait values;
//! it is gone when the job ends: a done job is deleted, and a job of this kind that dies is
//! scrubbed of the key (`jobs.rs`).
//!
//! `erasure_destination_calls` is keyed to the ledger and holds no person id, address or key.

use serde_json::json;
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::events::{SYNC_FAILED_EVENT, SYNC_SUCCEEDED_EVENT};
use zirv_flow_core::{TraceContext, WorkspaceId};

use crate::{NewJob, StoreError, enqueue};

/// The job kind that makes one destination's erase call.
pub const ERASE_CALL_JOB_KIND: &str = "destination.erase";

/// The payload key that holds the external key. A job that dies loses it.
pub const EXTERNAL_KEY_FIELD: &str = "external_key";

/// The attempts an erase call job gets. A wait for a rate token spends one too.
pub const ERASE_CALL_ATTEMPTS: i32 = 8;

/// The code of a call whose person had no value to send as the key.
pub const MISSING_KEY: &str = "missing_key";

/// Where the key a destination is given for a person comes from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExternalKey {
    /// The person's id (a built-in type that keys on it).
    PersonId,
    /// The person's identity of this kind.
    Identity(String),
    /// The person's value of this trait.
    Trait(String),
}

/// A destination the caller found to declare erase, and where its key comes from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EraseTarget {
    pub destination: Uuid,
    pub key: ExternalKey,
}

/// How a destination's call stands.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum EraseCallOutcome {
    Pending,
    Done,
    Failed,
}

impl EraseCallOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    fn parse(text: &str) -> Result<Self, StoreError> {
        match text {
            "pending" => Ok(Self::Pending),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            _ => Err(StoreError::query(sqlx::Error::Decode(
                "unknown erase call outcome".into(),
            ))),
        }
    }
}

/// One destination's erase call for an erasure request. No key, no person.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ErasureCall {
    pub erasure_id: Uuid,
    pub destination_id: Uuid,
    pub outcome: EraseCallOutcome,
    pub attempts: i32,
    /// A fixed code; set exactly when the call failed.
    pub error_code: Option<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

type CallRow = (
    Uuid,
    Uuid,
    String,
    i32,
    Option<String>,
    OffsetDateTime,
    OffsetDateTime,
);

/// Whether the destination handled any of `people`: a message or a forwarding item for them, or
/// the outcome event of a profile sync. (`delivery_attempts` names those items, never a person.)
async fn handled(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    destination: Uuid,
    people: &[Uuid],
) -> Result<bool, StoreError> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM messages WHERE workspace_id = $1 AND destination_id = $2 \
                          AND person_id = ANY($3)) \
             OR EXISTS (SELECT 1 FROM forwarding_outbox WHERE workspace_id = $1 \
                          AND destination_id = $2 AND person_id = ANY($3)) \
             OR EXISTS (SELECT 1 FROM events WHERE workspace_id = $1 AND person_id = ANY($3) \
                          AND name IN ($4, $5) AND properties ->> 'destination_id' = $2::text)",
    )
    .bind(workspace.as_uuid())
    .bind(destination)
    .bind(people)
    .bind(SYNC_SUCCEEDED_EVENT)
    .bind(SYNC_FAILED_EVENT)
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)
}

/// The external key as a destination is given it, from the person's rows as they are now.
async fn resolve(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    (current, people): (Uuid, &[Uuid]),
    key: &ExternalKey,
) -> Result<Option<String>, StoreError> {
    let value: Option<String> = match key {
        ExternalKey::PersonId => Some(current.to_string()),
        ExternalKey::Identity(kind) => sqlx::query_scalar(
            "SELECT value FROM identities WHERE workspace_id = $1 AND person_id = ANY($2) \
                AND kind = $3 ORDER BY value LIMIT 1",
        )
        .bind(workspace.as_uuid())
        .bind(people)
        .bind(kind)
        .fetch_optional(connection)
        .await
        .map_err(StoreError::query)?,
        ExternalKey::Trait(name) => sqlx::query_scalar::<_, Option<String>>(
            "SELECT traits ->> $3 FROM people WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace.as_uuid())
        .bind(current)
        .bind(name)
        .fetch_optional(connection)
        .await
        .map_err(StoreError::query)?
        .flatten(),
    };
    Ok(value
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty()))
}

/// The idempotency key of a ledger and destination pair: stable across every retry.
pub fn idempotency_key(erasure: Uuid, destination: Uuid) -> String {
    format!("erase:{erasure}:{destination}")
}

/// Writes the pending call and enqueues the job for every target that handled the person, in the
/// erasure transaction, before the person's rows are deleted. A target whose key cannot be
/// resolved is recorded `failed` with [`MISSING_KEY`] and gets no job.
pub(super) async fn plan(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    erasure: Uuid,
    (current, people): (Uuid, &[Uuid]),
    targets: &[EraseTarget],
    trace: &TraceContext,
) -> Result<(), StoreError> {
    for target in targets {
        if !handled(connection, workspace, target.destination, people).await? {
            continue;
        }
        let Some(external) = resolve(connection, workspace, (current, people), &target.key).await?
        else {
            sqlx::query(
                "INSERT INTO erasure_destination_calls \
                    (erasure_id, workspace_id, destination_id, outcome, error_code) \
                 VALUES ($1, $2, $3, 'failed', $4) ON CONFLICT DO NOTHING",
            )
            .bind(erasure)
            .bind(workspace.as_uuid())
            .bind(target.destination)
            .bind(MISSING_KEY)
            .execute(&mut *connection)
            .await
            .map_err(StoreError::query)?;
            continue;
        };
        let inserted = sqlx::query(
            "INSERT INTO erasure_destination_calls (erasure_id, workspace_id, destination_id) \
             VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(erasure)
        .bind(workspace.as_uuid())
        .bind(target.destination)
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?
        .rows_affected();
        if inserted == 0 {
            continue;
        }
        let mut job = NewJob::new(ERASE_CALL_JOB_KIND, trace.clone());
        job.workspace_id = Some(workspace);
        job.payload = json!({
            "workspace_id": workspace.as_uuid(),
            "ledger_id": erasure,
            "destination_id": target.destination,
            EXTERNAL_KEY_FIELD: external,
        });
        job.max_attempts = ERASE_CALL_ATTEMPTS;
        job.unique_key = Some(format!(
            "{ERASE_CALL_JOB_KIND}:{erasure}:{}",
            target.destination
        ));
        enqueue(&mut *connection, &job).await?;
    }
    Ok(())
}

/// Settles a pending call: `Ok` is done, `Err(code)` is failed with that fixed code. False when
/// the call is not pending (settled already, or not there).
pub async fn finish_erasure_call(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    (erasure, destination): (Uuid, Uuid),
    result: Result<(), &str>,
    attempts: i32,
) -> Result<bool, StoreError> {
    let (outcome, code) = match result {
        Ok(()) => (EraseCallOutcome::Done, None),
        Err(code) => (EraseCallOutcome::Failed, Some(code)),
    };
    let done = sqlx::query(
        "UPDATE erasure_destination_calls SET outcome = $4, attempts = $5, error_code = $6, \
            updated_at = now() \
         WHERE workspace_id = $1 AND erasure_id = $2 AND destination_id = $3 \
           AND outcome = 'pending'",
    )
    .bind(workspace.as_uuid())
    .bind(erasure)
    .bind(destination)
    .bind(outcome.as_str())
    .bind(attempts.max(0))
    .bind(code)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(done.rows_affected() == 1)
}

/// The calls of the given erasure requests, ordered by request and destination.
pub async fn list_erasure_calls(
    pool: &PgPool,
    workspace: WorkspaceId,
    erasures: &[Uuid],
) -> Result<Vec<ErasureCall>, StoreError> {
    let rows: Vec<CallRow> = sqlx::query_as(
        "SELECT erasure_id, destination_id, outcome, attempts, error_code, created_at, \
                updated_at \
         FROM erasure_destination_calls WHERE workspace_id = $1 AND erasure_id = ANY($2) \
         ORDER BY erasure_id, destination_id",
    )
    .bind(workspace.as_uuid())
    .bind(erasures)
    .fetch_all(pool)
    .await
    .map_err(StoreError::query)?;
    rows.into_iter()
        .map(
            |(
                erasure_id,
                destination_id,
                outcome,
                attempts,
                error_code,
                created_at,
                updated_at,
            )| {
                Ok(ErasureCall {
                    erasure_id,
                    destination_id,
                    outcome: EraseCallOutcome::parse(&outcome)?,
                    attempts,
                    error_code,
                    created_at,
                    updated_at,
                })
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use zirv_flow_core::people::{Identifier, identifier_hash};

    use super::*;
    use crate::privacy::testing::{EMAIL, Seeded, identifier_key, seed_everything, trace};
    use crate::{
        EraseOutcome, NewDestination, begin, commit, create_destination, erase_person,
        is_suppressed, request_erasure,
    };

    async fn destination(pool: &PgPool, ws: WorkspaceId, name: &str) -> Uuid {
        let id = Uuid::now_v7();
        create_destination(
            pool,
            ws,
            &NewDestination {
                id,
                kind: "fake",
                name,
                config: &json!({}),
                credential_id: None,
                rate: 10,
                burst: 10,
                created_by: None,
            },
        )
        .await
        .unwrap();
        id
    }

    /// A forwarding item of the person at the destination, in the given state.
    async fn outbox(pool: &PgPool, ws: WorkspaceId, destination: Uuid, person: Uuid, state: &str) {
        sqlx::query(
            "INSERT INTO forwarding_outbox (id, workspace_id, destination_id, rule_id, event_id, \
                person_id, idempotency_key, payload, state) \
             VALUES (gen_random_uuid(), $1, $2, gen_random_uuid(), gen_random_uuid(), $3, \
                gen_random_uuid()::text, '{\"plan\": \"pro\"}', $4)",
        )
        .bind(ws.as_uuid())
        .bind(destination)
        .bind(person)
        .bind(state)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn message(pool: &PgPool, ws: WorkspaceId, destination: Uuid, person: Uuid) {
        sqlx::query(
            "INSERT INTO messages (id, workspace_id, person_id, enrollment_id, node_id, \
                template_id, destination_id, status, reason, completed_at) \
             VALUES (gen_random_uuid(), $1, $3, gen_random_uuid(), 'n', gen_random_uuid(), $2, \
                'skipped', 'no_consent', now())",
        )
        .bind(ws.as_uuid())
        .bind(destination)
        .bind(person)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn sync_outcome(pool: &PgPool, ws: WorkspaceId, destination: Uuid, person: Uuid) {
        sqlx::query(
            "INSERT INTO events (id, workspace_id, person_id, name, occurred_at, received_at, \
                properties, source, trace_id, validation_state) \
             VALUES (gen_random_uuid(), $1, $3, $4, now(), now(), \
                jsonb_build_object('destination_id', $2::text), 'flows', gen_random_uuid(), \
                'valid')",
        )
        .bind(ws.as_uuid())
        .bind(destination)
        .bind(person)
        .bind(SYNC_SUCCEEDED_EVENT)
        .execute(pool)
        .await
        .unwrap();
    }

    struct Planned {
        seeded: Seeded,
        request: Uuid,
        messaged: Uuid,
        forwarded: Uuid,
        synced: Uuid,
        untouched: Uuid,
        keyless: Uuid,
        targets: Vec<EraseTarget>,
    }

    /// The seeded person with destinations that handled them (a message, a dead and a pending
    /// forwarding item, a sync outcome, and a message at a destination keyed on an identity the
    /// person lacks) and one that did not.
    async fn planned(pool: &PgPool) -> Planned {
        let seeded = seed_everything(pool).await;
        let (ws, person) = (seeded.ws, seeded.person);
        let messaged = destination(pool, ws, "messaged").await;
        let forwarded = destination(pool, ws, "forwarded").await;
        let synced = destination(pool, ws, "synced").await;
        let untouched = destination(pool, ws, "untouched").await;
        let keyless = destination(pool, ws, "keyless").await;
        message(pool, ws, messaged, person).await;
        outbox(pool, ws, forwarded, person, "dead").await;
        outbox(pool, ws, forwarded, seeded.merged, "pending").await;
        sync_outcome(pool, ws, synced, person).await;
        message(pool, ws, keyless, person).await;
        let targets = vec![
            EraseTarget {
                destination: messaged,
                key: ExternalKey::Identity("email".into()),
            },
            EraseTarget {
                destination: forwarded,
                key: ExternalKey::PersonId,
            },
            EraseTarget {
                destination: synced,
                key: ExternalKey::Trait("plan".into()),
            },
            EraseTarget {
                destination: untouched,
                key: ExternalKey::PersonId,
            },
            EraseTarget {
                destination: keyless,
                key: ExternalKey::Identity("phone".into()),
            },
        ];
        let mut tx = begin(pool).await.unwrap();
        let request = request_erasure(&mut tx, ws, person, &identifier_key(), None, &trace())
            .await
            .unwrap()
            .unwrap()
            .id;
        commit(tx).await.unwrap();
        Planned {
            seeded,
            request,
            messaged,
            forwarded,
            synced,
            untouched,
            keyless,
            targets,
        }
    }

    async fn erase(pool: &PgPool, plan: &Planned) -> EraseOutcome {
        let mut tx = begin(pool).await.unwrap();
        let outcome = erase_person(
            &mut tx,
            plan.seeded.ws,
            plan.request,
            &identifier_key(),
            &plan.targets,
            &trace(),
        )
        .await
        .unwrap();
        commit(tx).await.unwrap();
        outcome
    }

    async fn jobs(pool: &PgPool) -> Vec<(Option<String>, Value)> {
        sqlx::query_as("SELECT unique_key, payload FROM jobs WHERE kind = $1 ORDER BY unique_key")
            .bind(ERASE_CALL_JOB_KIND)
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[test]
    fn the_idempotency_key_is_stable_for_a_ledger_and_destination_pair() {
        let (ledger, one, two) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());

        assert_eq!(idempotency_key(ledger, one), idempotency_key(ledger, one));
        assert_ne!(idempotency_key(ledger, one), idempotency_key(ledger, two));
        assert_ne!(
            idempotency_key(ledger, one),
            idempotency_key(Uuid::now_v7(), one)
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn each_destination_that_handled_the_person_gets_one_pending_call_and_one_job(
        pool: PgPool,
    ) {
        let plan = planned(&pool).await;
        let ws = plan.seeded.ws;

        let outcome = erase(&pool, &plan).await;

        assert!(matches!(outcome, EraseOutcome::Completed { .. }));
        let calls = list_erasure_calls(&pool, ws, &[plan.request])
            .await
            .unwrap();
        let mut seen: Vec<(Uuid, EraseCallOutcome, Option<&str>)> = calls
            .iter()
            .map(|call| {
                (
                    call.destination_id,
                    call.outcome,
                    call.error_code.as_deref(),
                )
            })
            .collect();
        seen.sort();
        let mut want = vec![
            (plan.messaged, EraseCallOutcome::Pending, None),
            (plan.forwarded, EraseCallOutcome::Pending, None),
            (plan.synced, EraseCallOutcome::Pending, None),
            (plan.keyless, EraseCallOutcome::Failed, Some(MISSING_KEY)),
        ];
        want.sort();
        assert_eq!(seen, want, "the untouched destination has no row");
        assert!(calls.iter().all(|call| call.attempts == 0));
        let queued = jobs(&pool).await;
        let mut keys: Vec<String> = queued.iter().filter_map(|job| job.0.clone()).collect();
        keys.sort();
        let mut expected: Vec<String> = [plan.messaged, plan.forwarded, plan.synced]
            .iter()
            .map(|destination| format!("{ERASE_CALL_JOB_KIND}:{}:{destination}", plan.request))
            .collect();
        expected.sort();
        assert_eq!(
            keys, expected,
            "one job per kept target, none for the others"
        );
        assert!(queued.iter().all(|(_, payload)| payload["destination_id"]
            != json!(plan.untouched)
            && payload["destination_id"] != json!(plan.keyless)));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_job_payload_holds_the_external_key_and_no_trait_value(pool: PgPool) {
        let plan = planned(&pool).await;
        let ws = plan.seeded.ws;

        erase(&pool, &plan).await;

        let queued = jobs(&pool).await;
        for (_, payload) in &queued {
            let mut fields: Vec<&str> = payload
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            fields.sort();
            assert_eq!(
                fields,
                [
                    "destination_id",
                    "external_key",
                    "ledger_id",
                    "workspace_id"
                ]
            );
            assert_eq!(payload["ledger_id"], json!(plan.request));
            assert_eq!(payload["workspace_id"], json!(ws.as_uuid()));
            let text = payload.to_string();
            assert!(!text.contains("Oslo") && !text.contains("Ada"), "{text}");
        }
        let key_of = |destination: Uuid| {
            queued
                .iter()
                .find(|(_, payload)| payload["destination_id"] == json!(destination))
                .map(|(_, payload)| payload[EXTERNAL_KEY_FIELD].clone())
        };
        assert_eq!(key_of(plan.messaged), Some(json!(EMAIL)));
        assert_eq!(
            key_of(plan.forwarded),
            Some(json!(plan.seeded.person.to_string()))
        );
        assert_eq!(key_of(plan.synced), Some(json!("pro")));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_erasure_scan_finds_nothing_and_the_table_has_no_person_column(pool: PgPool) {
        let plan = planned(&pool).await;

        erase(&pool, &plan).await;

        for person in [plan.seeded.person, plan.seeded.merged] {
            assert_eq!(
                crate::people::erasure::rows_referencing_person(&pool, person).await,
                []
            );
        }
        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT column_name::text FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = 'erasure_destination_calls'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(
            columns.iter().all(|name| !name.contains("person")),
            "{columns:?}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn pending_and_dead_outbox_rows_are_gone_and_the_suppression_hash_remains(pool: PgPool) {
        let plan = planned(&pool).await;
        let ws = plan.seeded.ws;
        outbox(&pool, ws, plan.forwarded, plan.seeded.person, "pending").await;
        let group = vec![plan.seeded.person, plan.seeded.merged];
        let rows = |people: Vec<Uuid>| {
            let pool = pool.clone();
            async move {
                sqlx::query_as::<_, (String, i64)>(
                    "SELECT state, count(*) FROM forwarding_outbox WHERE person_id = ANY($1) \n                     GROUP BY state ORDER BY state",
                )
                .bind(people)
                .fetch_all(&pool)
                .await
                .unwrap()
            }
        };
        assert_eq!(
            rows(group.clone()).await,
            [("dead".to_owned(), 2), ("pending".to_owned(), 4)]
        );

        erase(&pool, &plan).await;

        assert_eq!(
            rows(group).await,
            [],
            "the dead and the pending rows are gone"
        );
        assert_eq!(
            rows(vec![plan.seeded.bystander]).await,
            [("delivered".to_owned(), 1)],
            "another person's rows stay"
        );
        let hash = identifier_hash(
            &identifier_key(),
            &Identifier::parse("email", EMAIL).unwrap(),
        );
        assert!(is_suppressed(&pool, ws, &hash).await.unwrap());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn erasing_again_plans_nothing_more(pool: PgPool) {
        let plan = planned(&pool).await;
        erase(&pool, &plan).await;

        let again = erase(&pool, &plan).await;

        assert_eq!(again, EraseOutcome::AlreadyCompleted);
        assert_eq!(jobs(&pool).await.len(), 3);
        let calls = list_erasure_calls(&pool, plan.seeded.ws, &[plan.request])
            .await
            .unwrap();
        assert_eq!(calls.len(), 4);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_pending_call_is_settled_once(pool: PgPool) {
        let plan = planned(&pool).await;
        let ws = plan.seeded.ws;
        erase(&pool, &plan).await;
        let mut connection = pool.acquire().await.unwrap();

        let done = finish_erasure_call(
            &mut connection,
            ws,
            (plan.request, plan.messaged),
            Ok(()),
            1,
        )
        .await
        .unwrap();
        let again = finish_erasure_call(
            &mut connection,
            ws,
            (plan.request, plan.messaged),
            Err("http_4xx"),
            2,
        )
        .await
        .unwrap();
        let failed = finish_erasure_call(
            &mut connection,
            ws,
            (plan.request, plan.synced),
            Err("http_4xx"),
            3,
        )
        .await
        .unwrap();
        drop(connection);

        assert!(done && !again && failed);
        let calls = list_erasure_calls(&pool, ws, &[plan.request])
            .await
            .unwrap();
        let find = |destination: Uuid| {
            calls
                .iter()
                .find(|call| call.destination_id == destination)
                .unwrap()
        };
        assert_eq!(find(plan.messaged).outcome, EraseCallOutcome::Done);
        assert_eq!(find(plan.messaged).attempts, 1);
        assert_eq!(find(plan.synced).outcome, EraseCallOutcome::Failed);
        assert_eq!(find(plan.synced).error_code.as_deref(), Some("http_4xx"));
        assert_eq!(find(plan.forwarded).outcome, EraseCallOutcome::Pending);
    }

    #[test]
    fn an_outcome_round_trips_and_an_unknown_one_is_refused() {
        for outcome in [
            EraseCallOutcome::Pending,
            EraseCallOutcome::Done,
            EraseCallOutcome::Failed,
        ] {
            assert_eq!(EraseCallOutcome::parse(outcome.as_str()).unwrap(), outcome);
        }
        assert!(EraseCallOutcome::parse("lost").is_err());
    }
}
