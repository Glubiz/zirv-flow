//! Gate 5, check and record: the frequency caps of a person.
//!
//! A message holds a *slot* against a person's caps from the moment gate 5 passes it
//! (`messages.cap_slot_at`) until it is skipped, or until a quiet-hours deferral releases it.
//! The check counts the slots of the person's other non-test messages inside each cap's window and
//! reserves one for this message, in a transaction of its own under a per-person advisory lock, so
//! two flows sending to one person at once can never both take the last slot. The transaction is
//! short and ends before the message is rendered or delivered; the send job holds no lock while it
//! calls the destination, and a retry of the job must not roll the reservation back.

use sqlx::PgPool;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use super::settings::CapMode;
use crate::{StoreError, begin, commit};

/// The class of the per-person cap lock: `pg_advisory_xact_lock(CAP_LOCK_CLASS, hash)` with a hash
/// of the workspace and the person. 7000 is the slot lock of the people module.
pub const CAP_LOCK_CLASS: i32 = 7001;

/// What the caps decided for a message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapVerdict {
    /// Every cap lets the message through, and it holds a slot.
    Reserved,
    /// A cap in `skip` mode is full: the message is skipped as `frequency_cap`.
    Skip,
    /// Only caps in `defer` mode are full: the message waits until the latest of the instants a
    /// slot leaves each of their windows.
    Defer { until: OffsetDateTime },
}

/// Checks the caps on `channel` for `message` and, when they let it through, reserves its slot
/// (at `now`).
///
/// The slots counted are those of the person's own other messages on the channel, not a test
/// send, not skipped, and reserved in `(now - window, now]`. A message that already holds a slot
/// (it ran before and waited for a rate-limit token) is not counted against itself and its slot
/// moves to `now`, so it counts from when it really sends; a message blocked reserves nothing.
pub async fn check_frequency_caps(
    pool: &PgPool,
    workspace: WorkspaceId,
    person: Uuid,
    message: Uuid,
    channel: &str,
    now: OffsetDateTime,
) -> Result<CapVerdict, StoreError> {
    let mut tx = begin(pool).await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2::text))")
        .bind(CAP_LOCK_CLASS)
        .bind(format!("{}:{person}", workspace.as_uuid()))
        .execute(&mut *tx)
        .await
        .map_err(StoreError::query)?;
    let caps: Vec<(i32, i32, String)> = sqlx::query_as(
        "SELECT window_hours, message_limit, mode FROM frequency_caps
         WHERE workspace_id = $1 AND channel = $2 ORDER BY id",
    )
    .bind(workspace.as_uuid())
    .bind(channel)
    .fetch_all(&mut *tx)
    .await
    .map_err(StoreError::query)?;

    let mut skip = false;
    let mut until: Option<OffsetDateTime> = None;
    for (window_hours, limit, mode) in caps {
        let held: i64 = sqlx::query_scalar(SLOTS_IN_WINDOW_COUNT)
            .bind(workspace.as_uuid())
            .bind(person)
            .bind(channel)
            .bind(now)
            .bind(window_hours)
            .bind(message)
            .fetch_one(&mut *tx)
            .await
            .map_err(StoreError::query)?;
        if held < i64::from(limit) {
            continue;
        }
        match CapMode::parse(&mode)? {
            CapMode::Skip => skip = true,
            CapMode::Defer => {
                // The message fits once enough slots have left the window that one is free: the
                // `held - limit + 1`th oldest, which is the oldest when the cap is exactly full.
                let leaving: Option<OffsetDateTime> = sqlx::query_scalar(SLOT_LEAVING)
                    .bind(workspace.as_uuid())
                    .bind(person)
                    .bind(channel)
                    .bind(now)
                    .bind(window_hours)
                    .bind(message)
                    .bind(held - i64::from(limit))
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(StoreError::query)?;
                let release = leaving.map_or(now, |slot| {
                    slot + time::Duration::hours(i64::from(window_hours))
                });
                until = Some(until.map_or(release, |earlier| earlier.max(release)));
            }
        }
    }
    let verdict = if skip {
        CapVerdict::Skip
    } else if let Some(until) = until {
        CapVerdict::Defer { until }
    } else {
        CapVerdict::Reserved
    };
    if verdict == CapVerdict::Reserved {
        sqlx::query("UPDATE messages SET cap_slot_at = $3 WHERE workspace_id = $1 AND id = $2")
            .bind(workspace.as_uuid())
            .bind(message)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::query)?;
    }
    commit(tx).await?;
    Ok(verdict)
}

const SLOTS_IN_WINDOW_COUNT: &str = "SELECT count(*) FROM messages
     WHERE workspace_id = $1 AND person_id = $2 AND channel = $3 AND NOT test
       AND cap_slot_at IS NOT NULL AND status <> 'skipped' AND id <> $6
       AND cap_slot_at > $4 - make_interval(hours => $5) AND cap_slot_at <= $4";

const SLOT_LEAVING: &str = "SELECT cap_slot_at FROM messages
     WHERE workspace_id = $1 AND person_id = $2 AND channel = $3 AND NOT test
       AND cap_slot_at IS NOT NULL AND status <> 'skipped' AND id <> $6
       AND cap_slot_at > $4 - make_interval(hours => $5) AND cap_slot_at <= $4
     ORDER BY cap_slot_at OFFSET $7 LIMIT 1";

/// Records that a gate deferred a pending message to `run_at`, as a `deferred` entry in its gate
/// results (the gate, the instant, and a note when the gate has one). With `release_slot` the same
/// write frees the slot the message holds, so the deferred run takes one when it really sends.
///
/// A statement of its own, so the entry outlives a job transaction that rolls back. Returns false
/// when the message is not pending (it ended meanwhile).
pub async fn record_message_deferral(
    pool: &PgPool,
    workspace: WorkspaceId,
    message: Uuid,
    gate: &str,
    run_at: OffsetDateTime,
    note: Option<&str>,
    release_slot: bool,
) -> Result<bool, StoreError> {
    let run_at = run_at.format(&Rfc3339).map_err(|_| {
        StoreError::query(sqlx::Error::Encode(
            "a time that cannot be formatted".into(),
        ))
    })?;
    let written = sqlx::query(
        "UPDATE messages SET
             gate_results = gate_results || jsonb_build_array(jsonb_strip_nulls(jsonb_build_object(
                 'gate', $3::text, 'result', 'deferred', 'run_at', $4::text, 'note', $5::text))),
             cap_slot_at = CASE WHEN $6 THEN NULL ELSE cap_slot_at END
         WHERE workspace_id = $1 AND id = $2 AND status = 'pending'",
    )
    .bind(workspace.as_uuid())
    .bind(message)
    .bind(gate)
    .bind(run_at)
    .bind(note)
    .bind(release_slot)
    .execute(pool)
    .await
    .map_err(StoreError::query)?
    .rows_affected();
    Ok(written == 1)
}

#[cfg(test)]
mod tests {
    use time::Duration;
    use time::macros::datetime;
    use zirv_flow_core::consent::Dimensions;
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::super::{NewFrequencyCap, replace_send_limits};
    use super::*;
    use crate::{MessageStatus, NewMessage, insert_message};

    const T0: OffsetDateTime = datetime!(2026-03-01 12:00 UTC);

    struct World {
        pool: PgPool,
        ws: WorkspaceId,
        person: Uuid,
    }

    async fn world(pool: PgPool, caps: &[(i32, i32, CapMode)]) -> World {
        crate::migrate(&pool).await.unwrap();
        let ws = crate::create_workspace(
            &pool,
            &WorkspaceName::parse("ours").unwrap(),
            &Slug::parse("ours").unwrap(),
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
        let caps: Vec<NewFrequencyCap> = caps
            .iter()
            .map(|(window_hours, limit, mode)| NewFrequencyCap {
                channel: "email".to_owned(),
                window_hours: *window_hours,
                limit: *limit,
                mode: *mode,
            })
            .collect();
        let mut conn = pool.acquire().await.unwrap();
        replace_send_limits(&mut conn, ws, 0, &caps, None)
            .await
            .unwrap();
        drop(conn);
        World { pool, ws, person }
    }

    impl World {
        async fn message(&self, status: MessageStatus) -> Uuid {
            let id = Uuid::now_v7();
            let dimensions = Dimensions::new();
            let reason = (status == MessageStatus::Skipped).then_some("no_consent");
            insert_message(
                &self.pool,
                &NewMessage {
                    id,
                    workspace: self.ws,
                    person: self.person,
                    enrollment: Uuid::now_v7(),
                    flow: None,
                    flow_version: None,
                    node: "mail",
                    template: Uuid::from_u128(1),
                    template_version: None,
                    theme_id: None,
                    theme_version: None,
                    snippet_versions: None,
                    destination: Uuid::from_u128(2),
                    sender_identity: None,
                    category: None,
                    topic: None,
                    dimensions: &dimensions,
                    status,
                    reason,
                    deciding_rule: None,
                    gates: &[],
                    attempts: 0,
                    now: T0,
                },
            )
            .await
            .unwrap();
            id
        }

        async fn check(&self, message: Uuid, now: OffsetDateTime) -> CapVerdict {
            check_frequency_caps(&self.pool, self.ws, self.person, message, "email", now)
                .await
                .unwrap()
        }

        async fn slot(&self, message: Uuid) -> Option<OffsetDateTime> {
            sqlx::query_scalar("SELECT cap_slot_at FROM messages WHERE id = $1")
                .bind(message)
                .fetch_one(&self.pool)
                .await
                .unwrap()
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_full_skip_cap_blocks_without_reserving_and_a_slot_leaves_with_its_window(
        pool: PgPool,
    ) {
        let world = world(pool, &[(24, 1, CapMode::Skip)]).await;
        let first = world.message(MessageStatus::Pending).await;
        let second = world.message(MessageStatus::Pending).await;

        assert_eq!(world.check(first, T0).await, CapVerdict::Reserved);
        assert_eq!(world.slot(first).await, Some(T0));
        assert_eq!(
            world.check(second, T0 + Duration::hours(1)).await,
            CapVerdict::Skip
        );
        assert_eq!(
            world.slot(second).await,
            None,
            "a blocked message reserves nothing"
        );
        assert_eq!(
            world.check(second, T0 + Duration::hours(24)).await,
            CapVerdict::Reserved,
            "the slot is out of the window exactly one window later"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_message_that_holds_a_slot_is_not_counted_against_itself(pool: PgPool) {
        let world = world(pool, &[(24, 1, CapMode::Skip)]).await;
        let first = world.message(MessageStatus::Pending).await;

        assert_eq!(world.check(first, T0).await, CapVerdict::Reserved);
        assert_eq!(
            world.check(first, T0 + Duration::hours(2)).await,
            CapVerdict::Reserved
        );
        assert_eq!(
            world.slot(first).await,
            Some(T0 + Duration::hours(2)),
            "one slot, moved to the re-run's time, not a second one"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_rerun_moves_its_slot_so_a_short_window_still_sees_it(pool: PgPool) {
        let world = world(pool, &[(1, 1, CapMode::Skip), (24, 5, CapMode::Skip)]).await;
        let m = world.message(MessageStatus::Pending).await;
        let n = world.message(MessageStatus::Pending).await;

        assert_eq!(world.check(m, T0).await, CapVerdict::Reserved);
        // M waited three hours for a rate-limit token and sends now.
        let sent = T0 + Duration::hours(3);
        assert_eq!(world.check(m, sent).await, CapVerdict::Reserved);
        assert_eq!(
            world.check(n, sent + Duration::minutes(10)).await,
            CapVerdict::Skip,
            "M's slot is inside the hourly window"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn skipped_and_released_messages_hold_no_slot_and_a_defer_cap_names_the_release(
        pool: PgPool,
    ) {
        let world = world(pool, &[(24, 2, CapMode::Defer)]).await;
        let a = world.message(MessageStatus::Pending).await;
        let b = world.message(MessageStatus::Pending).await;
        let c = world.message(MessageStatus::Pending).await;
        assert_eq!(world.check(a, T0).await, CapVerdict::Reserved);
        assert_eq!(
            world.check(b, T0 + Duration::hours(3)).await,
            CapVerdict::Reserved
        );

        // The cap is full: the third waits for the oldest slot, a's, to leave.
        assert_eq!(
            world.check(c, T0 + Duration::hours(4)).await,
            CapVerdict::Defer {
                until: T0 + Duration::hours(24)
            }
        );
        assert_eq!(world.slot(c).await, None);

        // A quiet-hours deferral of a releases its slot.
        assert!(
            record_message_deferral(&world.pool, world.ws, a, "quiet_hours", T0, None, true)
                .await
                .unwrap()
        );
        assert_eq!(world.slot(a).await, None);
        assert_eq!(
            world.check(c, T0 + Duration::hours(4)).await,
            CapVerdict::Reserved
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn several_caps_all_must_pass_and_skip_wins_over_defer(pool: PgPool) {
        let world = world(pool, &[(1, 1, CapMode::Defer), (24, 2, CapMode::Skip)]).await;
        let a = world.message(MessageStatus::Pending).await;
        let b = world.message(MessageStatus::Pending).await;
        let c = world.message(MessageStatus::Pending).await;
        assert_eq!(world.check(a, T0).await, CapVerdict::Reserved);
        // The hourly cap is full, the daily one is not.
        assert_eq!(
            world.check(b, T0 + Duration::minutes(10)).await,
            CapVerdict::Defer {
                until: T0 + Duration::hours(1)
            }
        );
        assert_eq!(
            world.check(b, T0 + Duration::minutes(70)).await,
            CapVerdict::Reserved
        );
        // Now both are full: the skip cap wins.
        assert_eq!(
            world.check(c, T0 + Duration::minutes(75)).await,
            CapVerdict::Skip
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_deferral_is_logged_and_survives_the_end_of_the_message(pool: PgPool) {
        let world = world(pool, &[]).await;
        let id = world.message(MessageStatus::Pending).await;
        let run_at = T0 + Duration::hours(9);

        record_message_deferral(
            &world.pool,
            world.ws,
            id,
            "quiet_hours",
            run_at,
            Some("timezone_fallback"),
            false,
        )
        .await
        .unwrap();
        let dimensions = Dimensions::new();
        insert_message(
            &world.pool,
            &NewMessage {
                id,
                workspace: world.ws,
                person: world.person,
                enrollment: Uuid::now_v7(),
                flow: None,
                flow_version: None,
                node: "mail",
                template: Uuid::from_u128(1),
                template_version: None,
                theme_id: None,
                theme_version: None,
                snippet_versions: None,
                destination: Uuid::from_u128(2),
                sender_identity: None,
                category: None,
                topic: None,
                dimensions: &dimensions,
                status: MessageStatus::Sent,
                reason: None,
                deciding_rule: None,
                gates: &[crate::GateRecord::passed("delivery")],
                attempts: 1,
                now: run_at,
            },
        )
        .await
        .unwrap();

        let gates: serde_json::Value =
            sqlx::query_scalar("SELECT gate_results FROM messages WHERE id = $1")
                .bind(id)
                .fetch_one(&world.pool)
                .await
                .unwrap();
        assert_eq!(gates[0]["gate"], "quiet_hours");
        assert_eq!(gates[0]["result"], "deferred");
        assert_eq!(gates[0]["note"], "timezone_fallback");
        assert_eq!(gates[0]["run_at"], "2026-03-01T21:00:00Z");
        assert_eq!(gates[1]["gate"], "delivery");
    }
}
