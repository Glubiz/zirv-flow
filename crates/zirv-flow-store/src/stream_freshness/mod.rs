//! Source freshness (architecture 7.5): the values a stream keeps (`last_success_at`,
//! `last_run_failed`, `enabled_at`), the one writer of them and the one read that turns them
//! into a verdict.
//!
//! The state is derived on every read, never stored. The API and the scheduler leader both call
//! [`stream_freshness`], so they cannot disagree. A record value never appears here.

mod outcome;
mod report;

pub use outcome::{StreamOutcome, record_stream_outcome};
pub use report::{StreamFreshness, stream_freshness};

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::{Duration, OffsetDateTime};
    use uuid::Uuid;
    use zirv_flow_core::{Freshness, WorkspaceId};

    use super::*;
    use crate::sync_runs::testing::{source_with_stream, workspace};
    use crate::{StreamSettings, configure_stream, database_now, find_stream_by_id};

    async fn report(
        pool: &PgPool,
        ws: Option<WorkspaceId>,
        source: Option<Uuid>,
        now: OffsetDateTime,
    ) -> Vec<StreamFreshness> {
        stream_freshness(pool, ws, source, now).await.unwrap()
    }

    async fn outcome(
        pool: &PgPool,
        ws: WorkspaceId,
        stream: Uuid,
        outcome: StreamOutcome,
        at: OffsetDateTime,
    ) -> bool {
        record_stream_outcome(&mut pool.acquire().await.unwrap(), ws, stream, outcome, at)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_outcome_succeeds_fails_recovers_and_never_moves_the_success_backwards(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;
        let (source, stream) = source_with_stream(&pool, ws, "crm", "full_refresh").await;
        let at = database_now(&pool).await.unwrap();
        let read = || async {
            let found = find_stream_by_id(&pool, ws, source, stream)
                .await
                .unwrap()
                .unwrap();
            (found.last_success_at, found.last_run_failed)
        };

        assert!(outcome(&pool, ws, stream, StreamOutcome::Failed, at).await);
        assert_eq!(read().await, (None, true));
        assert!(outcome(&pool, ws, stream, StreamOutcome::Succeeded, at).await);
        assert_eq!(read().await, (Some(at), false), "a success recovers");
        let earlier = at - Duration::hours(1);
        outcome(&pool, ws, stream, StreamOutcome::Succeeded, earlier).await;
        assert_eq!(read().await, (Some(at), false), "never backwards");
        let later = at + Duration::hours(1);
        outcome(&pool, ws, stream, StreamOutcome::Succeeded, later).await;
        assert_eq!(read().await, (Some(later), false));
        outcome(&pool, ws, stream, StreamOutcome::Failed, later).await;
        assert_eq!(
            read().await,
            (Some(later), true),
            "a failure keeps the success"
        );
        assert!(
            !outcome(&pool, other, stream, StreamOutcome::Succeeded, later).await,
            "another workspace reports on nothing"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_state_follows_the_threshold_the_schedule_and_the_last_outcome(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let (source, stream) = source_with_stream(&pool, ws, "crm", "full_refresh").await;
        let enabled_at: OffsetDateTime =
            sqlx::query_scalar("SELECT enabled_at FROM streams WHERE id = $1")
                .bind(stream)
                .fetch_one(&pool)
                .await
                .unwrap();
        let state = |now| {
            let pool = pool.clone();
            async move { report(&pool, Some(ws), Some(source), now).await.remove(0) }
        };

        // No schedule and no threshold: never stale.
        let none = state(enabled_at + Duration::days(400)).await;
        assert_eq!(none.state, Freshness::Fresh);
        assert_eq!(
            (none.threshold_seconds, none.threshold_is_default),
            (None, false)
        );

        // The schedule gives the default: twice the hourly interval.
        sqlx::query("UPDATE sources SET schedule = '0 * * * *'")
            .execute(&pool)
            .await
            .unwrap();
        let inside = state(enabled_at + Duration::hours(2)).await;
        assert_eq!(
            inside.state,
            Freshness::Fresh,
            "equal to the threshold is fresh"
        );
        assert_eq!(inside.threshold_seconds, Some(7200));
        assert!(inside.threshold_is_default);
        let past = state(enabled_at + Duration::hours(2) + Duration::seconds(1)).await;
        assert_eq!(past.state, Freshness::Stale);
        assert_eq!(past.last_success_at, None);
        assert_eq!(past.age_seconds, 7201);

        // An explicit threshold wins over the schedule.
        sqlx::query("UPDATE streams SET staleness_threshold_seconds = 60")
            .execute(&pool)
            .await
            .unwrap();
        let explicit = state(enabled_at + Duration::seconds(61)).await;
        assert_eq!(explicit.state, Freshness::Stale);
        assert_eq!(explicit.threshold_seconds, Some(60));
        assert!(!explicit.threshold_is_default);

        // A success restarts the age; a failure beats the age.
        let success = enabled_at + Duration::minutes(10);
        outcome(&pool, ws, stream, StreamOutcome::Succeeded, success).await;
        assert_eq!(
            state(success + Duration::seconds(60)).await.state,
            Freshness::Fresh
        );
        assert_eq!(
            state(success + Duration::seconds(61)).await.state,
            Freshness::Stale
        );
        outcome(&pool, ws, stream, StreamOutcome::Failed, success).await;
        assert_eq!(state(success).await.state, Freshness::Failing);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_disabled_stream_and_a_stream_of_a_disabled_source_are_not_reported(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let (disabled_source, _) = source_with_stream(&pool, ws, "off", "full_refresh").await;
        let (source, _) = source_with_stream(&pool, ws, "crm", "full_refresh").await;
        let now = database_now(&pool).await.unwrap();
        assert_eq!(report(&pool, Some(ws), None, now).await.len(), 2);
        sqlx::query("UPDATE sources SET enabled = false WHERE id = $1")
            .bind(disabled_source)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(report(&pool, Some(ws), None, now).await.len(), 1);

        configure_stream(
            &mut pool.acquire().await.unwrap(),
            ws,
            source,
            "contacts",
            &StreamSettings {
                enabled: false,
                sync_mode: "full_refresh",
                cursor_field: None,
                primary_key: &[],
                staleness_threshold_seconds: None,
            },
        )
        .await
        .unwrap();
        assert!(report(&pool, Some(ws), None, now).await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_report_filters_by_workspace_and_source_and_carries_the_checkpoint(pool: PgPool) {
        let (one, two) = (workspace(&pool, "one").await, workspace(&pool, "two").await);
        let (source_one, stream_one) = source_with_stream(&pool, one, "crm", "full_refresh").await;
        let (source_two, stream_two) = source_with_stream(&pool, two, "crm", "full_refresh").await;
        sqlx::query("UPDATE streams SET checkpoint = $2 WHERE id = $1")
            .bind(stream_one)
            .bind(json!({"offset": 5}))
            .execute(&pool)
            .await
            .unwrap();
        let now = database_now(&pool).await.unwrap();

        let all = report(&pool, None, None, now).await;
        let own = report(&pool, Some(one), None, now).await;
        let by_source = report(&pool, Some(one), Some(source_one), now).await;
        let foreign_source = report(&pool, Some(one), Some(source_two), now).await;
        let other = report(&pool, Some(two), None, now).await;

        assert_eq!(all.len(), 2);
        assert_eq!(own.len(), 1);
        assert_eq!(own[0].stream_id, stream_one);
        assert_eq!(own[0].workspace_id, one.as_uuid());
        assert_eq!(own[0].high_water_mark, Some(json!({"offset": 5})));
        assert_eq!(by_source, own);
        assert!(foreign_source.is_empty(), "a source of another workspace");
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].stream_id, stream_two);
        assert_eq!(other[0].high_water_mark, None);
    }
}
