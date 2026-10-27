//! Message retention (architecture section 16.3): terminal `messages` rows past their window are
//! deleted in batches, and each batch writes the handled markers of the rows it removes in the
//! same statement (`privacy::delete_and_mark`), so a purge never loses the fact that a
//! destination handled a person. Every batch commits on its own and a rerun is idempotent.

use hashbrown::HashMap;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::StoreError;
use crate::events::RETENTION_BATCH;

/// The operator's message retention settings, in days.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MessageRetention {
    /// How long ended messages are kept unless their workspace has a shorter window.
    pub days: u32,
    /// The shortest and the longest window a workspace may set.
    pub min_days: u32,
    pub max_days: u32,
}

impl Default for MessageRetention {
    fn default() -> Self {
        Self {
            days: 395,
            min_days: 30,
            max_days: 1095,
        }
    }
}

/// What one run removed. Counts only: no ids of people or destinations.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MessageRetentionReport {
    pub messages_deleted: u64,
}

/// Deletes the terminal messages of one batch, oldest first, and writes their markers. `$1` is
/// now, `$2` the batch size, `$3`/`$4` the workspaces with their own window and its days, `$5`
/// the default days and `$6` the shortest of all windows, which bounds the index scan. A message
/// still `pending` or `sending` belongs to a live job and stays.
fn batch_statement() -> String {
    crate::privacy::delete_and_mark(
        "DELETE FROM messages m USING ( \
            SELECT x.workspace_id, x.id FROM messages x \
            WHERE x.status IN ('sent', 'skipped', 'failed') \
              AND x.created_at < $1 - make_interval(days => $6) \
              AND x.created_at < $1 - make_interval(days => COALESCE( \
                    (SELECT o.days FROM unnest($3::uuid[], $4::int[]) AS o(workspace_id, days) \
                      WHERE o.workspace_id = x.workspace_id), $5)) \
            ORDER BY x.created_at, x.id LIMIT $2) d \
         WHERE m.workspace_id = d.workspace_id AND m.id = d.id \
         RETURNING m.workspace_id, m.person_id, m.destination_id::text AS destination, \
                   m.created_at AS handled_at",
    )
}

/// Applies the retention as of `now` at the deployment window.
pub async fn apply_message_retention(
    pool: &PgPool,
    settings: &MessageRetention,
    now: OffsetDateTime,
) -> Result<MessageRetentionReport, StoreError> {
    apply_message_retention_for(pool, settings, &HashMap::new(), now).await
}

/// Like [`apply_message_retention`], with their own window in days for the workspaces in
/// `workspace_days`. A workspace not in the map keeps the deployment window.
pub async fn apply_message_retention_for(
    pool: &PgPool,
    settings: &MessageRetention,
    workspace_days: &HashMap<WorkspaceId, u32>,
    now: OffsetDateTime,
) -> Result<MessageRetentionReport, StoreError> {
    purge_in_batches(pool, settings, workspace_days, now, RETENTION_BATCH).await
}

async fn purge_in_batches(
    pool: &PgPool,
    settings: &MessageRetention,
    workspace_days: &HashMap<WorkspaceId, u32>,
    now: OffsetDateTime,
    batch_size: i64,
) -> Result<MessageRetentionReport, StoreError> {
    let days = |value: u32| i32::try_from(value).unwrap_or(i32::MAX);
    let (workspaces, windows): (Vec<Uuid>, Vec<i32>) = workspace_days
        .iter()
        .map(|(workspace, value)| (workspace.as_uuid(), days(*value)))
        .unzip();
    let shortest = windows.iter().copied().fold(days(settings.days), i32::min);
    let statement = batch_statement();
    let mut report = MessageRetentionReport::default();
    loop {
        let deleted: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(statement.clone()))
            .bind(now)
            .bind(batch_size)
            .bind(&workspaces)
            .bind(&windows)
            .bind(days(settings.days))
            .bind(shortest)
            .fetch_one(pool)
            .await
            .map_err(StoreError::query)?;
        report.messages_deleted += u64::try_from(deleted).unwrap_or(0);
        if deleted < batch_size {
            return Ok(report);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::Duration;
    use time::macros::datetime;

    use super::*;
    use crate::privacy::testing::seed_everything;
    use crate::{NewDestination, create_destination};

    const NOW: OffsetDateTime = datetime!(2026-06-01 12:00 UTC);

    fn settings() -> MessageRetention {
        MessageRetention {
            days: 100,
            min_days: 7,
            max_days: 1000,
        }
    }

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

    /// Stores a message `age_days` before NOW with the given status and node, and returns its id.
    async fn message(
        pool: &PgPool,
        (ws, person, destination): (WorkspaceId, Uuid, Uuid),
        age_days: i64,
        status: &str,
        node: &str,
    ) -> Uuid {
        let id = Uuid::now_v7();
        let created = NOW - Duration::days(age_days);
        sqlx::query(
            "INSERT INTO messages (id, workspace_id, person_id, enrollment_id, node_id, \
                template_id, destination_id, status, reason, created_at, completed_at) \
             VALUES ($1, $2, $3, gen_random_uuid(), $4, gen_random_uuid(), $5, $6, \
                CASE WHEN $6 IN ('skipped', 'failed') THEN 'no_consent' END, $7, \
                CASE WHEN $6 IN ('pending', 'sending') THEN NULL ELSE $7 END)",
        )
        .bind(id)
        .bind(ws.as_uuid())
        .bind(person)
        .bind(node)
        .bind(destination)
        .bind(status)
        .bind(created)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    async fn link(pool: &PgPool, ws: WorkspaceId, message: Uuid) {
        sqlx::query(
            "INSERT INTO message_links (workspace_id, message_id, link_index, url) \
             VALUES ($1, $2, 0, 'https://example.com')",
        )
        .bind(ws.as_uuid())
        .bind(message)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn count(pool: &PgPool, table: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn nodes(pool: &PgPool) -> Vec<String> {
        sqlx::query_scalar("SELECT node_id FROM messages ORDER BY node_id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    /// A seeded workspace with its messages cleared, a person and a destination.
    async fn world(pool: &PgPool) -> (WorkspaceId, Uuid, Uuid) {
        let seeded = seed_everything(pool).await;
        sqlx::query("DELETE FROM messages")
            .execute(pool)
            .await
            .unwrap();
        let destination = destination(pool, seeded.ws, "mail").await;
        (seeded.ws, seeded.person, destination)
    }

    async fn run(pool: &PgPool) -> MessageRetentionReport {
        apply_message_retention(pool, &settings(), NOW)
            .await
            .unwrap()
    }

    #[test]
    fn the_defaults_are_13_months_and_30_to_1095_day_windows() {
        let defaults = MessageRetention::default();

        assert_eq!(
            (defaults.days, defaults.min_days, defaults.max_days),
            (395, 30, 1095)
        );
    }

    #[test]
    fn the_batch_statement_marks_what_it_deletes_and_names_the_terminal_statuses() {
        let sql = batch_statement();

        assert!(sql.starts_with("WITH gone AS (DELETE FROM messages m"));
        assert!(sql.contains("status IN ('sent', 'skipped', 'failed')"));
        assert!(sql.contains("ORDER BY x.created_at, x.id LIMIT $2"));
        assert!(sql.contains("INSERT INTO erasure_handled_markers"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_message_past_the_window_goes_with_its_links_and_one_inside_it_stays(pool: PgPool) {
        let world = world(&pool).await;
        let old = message(&pool, world, 101, "sent", "old").await;
        message(&pool, world, 99, "sent", "new").await;
        link(&pool, world.0, old).await;

        let report = run(&pool).await;

        assert_eq!(report.messages_deleted, 1);
        assert_eq!(nodes(&pool).await, ["new"]);
        assert_eq!(count(&pool, "message_links").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_pending_or_sending_message_past_the_window_stays_and_every_ended_one_goes(
        pool: PgPool,
    ) {
        let world = world(&pool).await;
        for (status, node) in [
            ("pending", "a-pending"),
            ("sending", "b-sending"),
            ("sent", "c-sent"),
            ("skipped", "d-skipped"),
            ("failed", "e-failed"),
        ] {
            message(&pool, world, 500, status, node).await;
        }

        let report = run(&pool).await;

        assert_eq!(report.messages_deleted, 3);
        assert_eq!(nodes(&pool).await, ["a-pending", "b-sending"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_purge_writes_one_marker_per_person_and_destination_with_the_earliest_time(
        pool: PgPool,
    ) {
        let (ws, person, one) = world(&pool).await;
        let two = destination(&pool, ws, "other").await;
        for (destination, age, status) in [
            (one, 300, "sent"),
            (one, 200, "failed"),
            (one, 150, "skipped"),
            (two, 120, "sent"),
        ] {
            message(&pool, (ws, person, destination), age, status, "m").await;
        }

        run(&pool).await;

        let markers: Vec<(Uuid, OffsetDateTime)> = sqlx::query_as(
            "SELECT destination_id, first_handled_at FROM erasure_handled_markers \
             WHERE person_id = $1 AND destination_id IN ($2, $3) ORDER BY first_handled_at",
        )
        .bind(person)
        .bind(one)
        .bind(two)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            markers,
            [
                (one, NOW - Duration::days(300)),
                (two, NOW - Duration::days(120))
            ]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_purge_moves_an_existing_marker_back_to_an_older_message(pool: PgPool) {
        let (ws, person, one) = world(&pool).await;
        sqlx::query(
            "INSERT INTO erasure_handled_markers \
                (workspace_id, person_id, destination_id, first_handled_at) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(ws.as_uuid())
        .bind(person)
        .bind(one)
        .bind(NOW - Duration::days(100))
        .execute(&pool)
        .await
        .unwrap();
        message(&pool, (ws, person, one), 300, "sent", "m").await;

        run(&pool).await;

        let first: OffsetDateTime = sqlx::query_scalar(
            "SELECT first_handled_at FROM erasure_handled_markers \
             WHERE person_id = $1 AND destination_id = $2",
        )
        .bind(person)
        .bind(one)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(first, NOW - Duration::days(300));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_second_run_deletes_and_marks_nothing(pool: PgPool) {
        let world = world(&pool).await;
        message(&pool, world, 300, "sent", "m").await;
        assert_eq!(run(&pool).await.messages_deleted, 1);
        let markers = count(&pool, "erasure_handled_markers").await;

        let again = run(&pool).await;

        assert_eq!(again, MessageRetentionReport::default());
        assert_eq!(count(&pool, "erasure_handled_markers").await, markers);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_message_to_a_destination_that_is_gone_is_deleted_without_a_marker(pool: PgPool) {
        let (ws, person, _) = world(&pool).await;
        let before = count(&pool, "erasure_handled_markers").await;
        message(&pool, (ws, person, Uuid::now_v7()), 300, "sent", "m").await;

        assert_eq!(run(&pool).await.messages_deleted, 1);

        assert_eq!(count(&pool, "erasure_handled_markers").await, before);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn more_rows_than_a_batch_purge_in_several_commits(pool: PgPool) {
        let world = world(&pool).await;
        for index in 0..5 {
            message(&pool, world, 300 - index, "sent", &format!("m{index}")).await;
        }
        message(&pool, world, 1, "sent", "fresh").await;

        let report = purge_in_batches(&pool, &settings(), &HashMap::new(), NOW, 2)
            .await
            .unwrap();

        assert_eq!(report.messages_deleted, 5);
        assert_eq!(nodes(&pool).await, ["fresh"]);
    }

    /// Makes the delete of the message at `node` fail, as a constraint would.
    async fn poison(pool: &PgPool, node: &str) {
        sqlx::query(
            "CREATE FUNCTION refuse() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN RAISE EXCEPTION 'refused'; END $$",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE TRIGGER refuse BEFORE DELETE ON messages FOR EACH ROW \
             WHEN (OLD.node_id = '{node}') EXECUTE FUNCTION refuse()"
        )))
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failure_in_batch_two_keeps_batch_one_and_the_rerun_continues(pool: PgPool) {
        let world = world(&pool).await;
        // Oldest first: m0 and m1 are batch one; m2 (refused) and m3 are batch two.
        for index in 0..4 {
            message(&pool, world, 300 - index, "sent", &format!("m{index}")).await;
        }
        poison(&pool, "m2").await;

        let failed = purge_in_batches(&pool, &settings(), &HashMap::new(), NOW, 2).await;

        assert!(failed.is_err());
        assert_eq!(nodes(&pool).await, ["m2", "m3"], "batch one is committed");
        sqlx::query("DROP TRIGGER refuse ON messages")
            .execute(&pool)
            .await
            .unwrap();
        let rerun = purge_in_batches(&pool, &settings(), &HashMap::new(), NOW, 2)
            .await
            .unwrap();
        assert_eq!(rerun.messages_deleted, 2, "only what the rerun removed");
        assert!(nodes(&pool).await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_marker_that_cannot_be_written_fails_the_whole_batch_and_the_rows_come_back(
        pool: PgPool,
    ) {
        let world = world(&pool).await;
        let markers = count(&pool, "erasure_handled_markers").await;
        for index in 0..2 {
            message(&pool, world, 300 - index, "sent", &format!("m{index}")).await;
        }
        // What a person erased mid-statement leaves: the marker insert is refused.
        sqlx::query(
            "CREATE FUNCTION refuse() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN RAISE EXCEPTION 'person gone'; END $$",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER refuse BEFORE INSERT ON erasure_handled_markers \
             FOR EACH ROW EXECUTE FUNCTION refuse()",
        )
        .execute(&pool)
        .await
        .unwrap();

        let failed = apply_message_retention(&pool, &settings(), NOW).await;

        assert!(failed.is_err());
        assert_eq!(nodes(&pool).await, ["m0", "m1"], "the deletes rolled back");
        sqlx::query("DROP TRIGGER refuse ON erasure_handled_markers")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(run(&pool).await.messages_deleted, 2);
        assert_eq!(count(&pool, "erasure_handled_markers").await, markers + 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_workspace_with_a_shorter_window_purges_sooner_and_others_keep_the_default(
        pool: PgPool,
    ) {
        let world = world(&pool).await;
        message(&pool, world, 40, "sent", "forty").await;
        let elsewhere = HashMap::from([(WorkspaceId::from_uuid(Uuid::now_v7()), 30_u32)]);

        let untouched = apply_message_retention_for(&pool, &settings(), &elsewhere, NOW)
            .await
            .unwrap();
        let own = HashMap::from([(world.0, 30_u32)]);
        let purged = apply_message_retention_for(&pool, &settings(), &own, NOW)
            .await
            .unwrap();

        assert_eq!(untouched.messages_deleted, 0);
        assert_eq!(purged.messages_deleted, 1);
    }
}
