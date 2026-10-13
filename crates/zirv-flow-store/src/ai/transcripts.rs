//! Opt-in transcripts: the request and response parts of a call, kept while the workspace's admin
//! has asked for it, for 1 to 30 days.

use serde_json::Value;
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{UserId, WorkspaceId};

use crate::StoreError;

/// The most transcripts one purge deletes; the job repeats until a pass finds fewer.
pub const TRANSCRIPT_PURGE_BATCH: i64 = 1000;

/// Stores a transcript of a recorded call, to be purged at `expires_at`.
pub async fn insert_ai_transcript(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    call: Uuid,
    user: Option<UserId>,
    content: &Value,
    expires_at: OffsetDateTime,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO ai_transcripts (id, workspace_id, call_id, user_id, content, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(Uuid::now_v7())
    .bind(workspace.as_uuid())
    .bind(call)
    .bind(user.map(|id| id.as_uuid()))
    .bind(content)
    .bind(expires_at)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// Deletes up to `limit` transcripts that expired at or before `now`, across all workspaces, and
/// returns how many. Rows another purge holds are skipped, so concurrent runs do not collide.
pub async fn purge_expired_ai_transcripts(
    connection: &mut PgConnection,
    now: OffsetDateTime,
    limit: i64,
) -> Result<u64, StoreError> {
    let done = sqlx::query(
        "DELETE FROM ai_transcripts WHERE id IN ( \
            SELECT id FROM ai_transcripts WHERE expires_at <= $1 \
            ORDER BY expires_at LIMIT $2 FOR UPDATE SKIP LOCKED)",
    )
    .bind(now)
    .bind(limit)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(done.rows_affected())
}

/// Deletes the workspace's transcripts whose content contains any of `needles`, ignoring case:
/// the erased person's ids and identifier values. A transcript is free text, so it is matched by
/// substring, which can delete a transcript that merely contains a short value. Returns how many
/// were deleted.
pub async fn erase_ai_transcripts(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    needles: &[String],
) -> Result<u64, StoreError> {
    let needles: Vec<&str> = needles
        .iter()
        .map(|needle| needle.trim())
        .filter(|needle| !needle.is_empty())
        .collect();
    if needles.is_empty() {
        return Ok(0);
    }
    let done = sqlx::query(
        "DELETE FROM ai_transcripts t WHERE t.workspace_id = $1 AND EXISTS ( \
            SELECT 1 FROM unnest($2::text[]) AS needle \
            WHERE position(lower(needle) IN lower(t.content::text)) > 0)",
    )
    .bind(workspace.as_uuid())
    .bind(&needles)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(done.rows_affected())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::Duration;
    use time::macros::datetime;
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;
    use crate::ai::{CallOutcome, NewAiCall, record_ai_call};
    use crate::create_workspace;

    async fn workspace(pool: &PgPool, name: &str) -> WorkspaceId {
        let name_parsed = WorkspaceName::parse(name).unwrap();
        let slug = Slug::parse(name).unwrap();
        create_workspace(pool, &name_parsed, &slug)
            .await
            .unwrap()
            .id
    }

    async fn transcript(
        pool: &PgPool,
        ws: WorkspaceId,
        content: Value,
        expires_at: OffsetDateTime,
    ) -> Uuid {
        let call = Uuid::now_v7();
        let mut connection = pool.acquire().await.unwrap();
        record_ai_call(
            &mut connection,
            &NewAiCall {
                id: call,
                workspace_id: ws,
                user_id: None,
                feature: "mapping",
                provider_id: "p",
                model: "m",
                input_tokens: 1,
                output_tokens: 1,
                estimated_cost_micros: 1,
                latency_ms: 1,
                outcome: CallOutcome::Ok,
                draft_id: None,
            },
        )
        .await
        .unwrap();
        insert_ai_transcript(&mut connection, ws, call, None, &content, expires_at)
            .await
            .unwrap();
        call
    }

    async fn count(pool: &PgPool, table: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_purge_removes_only_expired_transcripts_and_is_idempotent(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let now = datetime!(2026-05-15 12:00 UTC);
        transcript(&pool, ws, json!({"n": 1}), now - Duration::days(1)).await;
        transcript(&pool, ws, json!({"n": 2}), now).await;
        transcript(&pool, ws, json!({"n": 3}), now + Duration::days(1)).await;
        let mut connection = pool.acquire().await.unwrap();

        let first = purge_expired_ai_transcripts(&mut connection, now, TRANSCRIPT_PURGE_BATCH)
            .await
            .unwrap();
        let second = purge_expired_ai_transcripts(&mut connection, now, TRANSCRIPT_PURGE_BATCH)
            .await
            .unwrap();

        assert_eq!((first, second), (2, 0));
        let left: Vec<Value> = sqlx::query_scalar("SELECT content FROM ai_transcripts")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(left, [json!({"n": 3})]);
        // The call log row stays: only the content expires.
        assert_eq!(count(&pool, "ai_calls").await, 3);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_purge_is_bounded_by_its_batch(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let now = datetime!(2026-05-15 12:00 UTC);
        for n in 0..5 {
            transcript(&pool, ws, json!({"n": n}), now - Duration::days(1)).await;
        }
        let mut connection = pool.acquire().await.unwrap();

        let first = purge_expired_ai_transcripts(&mut connection, now, 3)
            .await
            .unwrap();
        let second = purge_expired_ai_transcripts(&mut connection, now, 3)
            .await
            .unwrap();
        assert_eq!((first, second), (3, 2));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn erasure_deletes_only_the_transcripts_that_name_the_person_in_that_workspace(
        pool: PgPool,
    ) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let later = OffsetDateTime::now_utc() + Duration::days(7);
        transcript(&pool, one, json!({"user": "Mapped ADA@Example.com"}), later).await;
        transcript(&pool, one, json!({"person": "0190-aaaa"}), later).await;
        transcript(&pool, one, json!({"user": "bob@example.com"}), later).await;
        transcript(&pool, two, json!({"user": "ada@example.com"}), later).await;
        let mut connection = pool.acquire().await.unwrap();
        let needles = ["ada@example.com", "0190-aaaa", " "].map(str::to_owned);

        let deleted = erase_ai_transcripts(&mut connection, one, &needles)
            .await
            .unwrap();

        assert_eq!(deleted, 2);
        assert_eq!(count(&pool, "ai_transcripts").await, 2);
        let nothing = erase_ai_transcripts(&mut connection, one, &[String::new()])
            .await
            .unwrap();
        assert_eq!(nothing, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn transcripts_are_isolated_per_workspace_and_go_with_their_call(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let later = OffsetDateTime::now_utc() + Duration::days(7);
        let call = transcript(&pool, one, json!({"a": 1}), later).await;
        transcript(&pool, two, json!({"b": 2}), later).await;

        let seen: i64 =
            sqlx::query_scalar("SELECT count(*) FROM ai_transcripts WHERE workspace_id = $1")
                .bind(two.as_uuid())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(seen, 1);

        sqlx::query("DELETE FROM ai_calls WHERE id = $1")
            .bind(call)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(count(&pool, "ai_transcripts").await, 1);
    }
}
