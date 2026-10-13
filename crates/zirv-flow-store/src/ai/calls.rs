//! The call log: one content-free row per dispatched model call, and the counts the gates read
//! from it. No column holds a prompt, a response or an error body.

use sqlx::PgConnection;
use sqlx::PgExecutor;
use sqlx::prelude::FromRow;
use time::{OffsetDateTime, Time, UtcOffset};
use uuid::Uuid;
use zirv_flow_core::{UserId, WorkspaceId};

use crate::StoreError;

/// How a dispatched call ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallOutcome {
    /// Reserved before the request and not finished yet. A process that dies mid-call leaves it
    /// at its worst-case cost.
    Pending,
    /// Given up on while pending: the handler panicked or the process died mid-call. It keeps its
    /// worst-case cost, because whether the provider billed the call is unknown.
    Abandoned,
    Ok,
    HttpError,
    Timeout,
    EgressDenied,
    Transport,
    Malformed,
    Capability,
}

impl CallOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Abandoned => "abandoned",
            Self::Ok => "ok",
            Self::HttpError => "http_error",
            Self::Timeout => "timeout",
            Self::EgressDenied => "egress_denied",
            Self::Transport => "transport",
            Self::Malformed => "malformed",
            Self::Capability => "capability",
        }
    }
}

#[derive(Clone, Debug)]
pub struct NewAiCall<'a> {
    pub id: Uuid,
    pub workspace_id: WorkspaceId,
    pub user_id: Option<UserId>,
    pub feature: &'a str,
    pub provider_id: &'a str,
    pub model: &'a str,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub estimated_cost_micros: i64,
    pub latency_ms: u32,
    pub outcome: CallOutcome,
    pub draft_id: Option<Uuid>,
}

/// Records a dispatched call.
pub async fn record_ai_call(
    connection: &mut PgConnection,
    call: &NewAiCall<'_>,
) -> Result<(), StoreError> {
    let clamp = |tokens: u32| i32::try_from(tokens).unwrap_or(i32::MAX);
    sqlx::query(
        "INSERT INTO ai_calls (id, workspace_id, user_id, feature, provider_id, model, \
             input_tokens, output_tokens, estimated_cost_micros, latency_ms, outcome, draft_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(call.id)
    .bind(call.workspace_id.as_uuid())
    .bind(call.user_id.map(|id| id.as_uuid()))
    .bind(call.feature)
    .bind(call.provider_id)
    .bind(call.model)
    .bind(clamp(call.input_tokens))
    .bind(clamp(call.output_tokens))
    .bind(call.estimated_cost_micros.max(0))
    .bind(clamp(call.latency_ms))
    .bind(call.outcome.as_str())
    .bind(call.draft_id)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// What a reservation came to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reservation {
    /// A `pending` row was recorded; the request may be sent.
    Granted,
    /// The user already made the limit of calls in the last minute.
    RateLimited,
    /// The workspace has already spent its monthly budget.
    BudgetExceeded,
}

/// What a reservation is checked against.
#[derive(Clone, Copy, Debug)]
pub struct ReservationLimits {
    /// Calls one user may make in the last minute.
    pub rate_limit: i64,
    /// The monthly budget. A call is admitted only when the month's spend plus its worst-case cost
    /// stays within it, so real spend never exceeds the budget.
    pub budget_micros: i64,
    /// Seconds after which a still-pending call of the workspace counts as abandoned.
    pub stale_after_secs: f64,
}

/// Checks the rate limit and the budget and, when both admit the call, records it as `pending` at
/// `call.estimated_cost_micros` (its worst-case cost), all under a lock on the workspace that is
/// held until the transaction ends. Counting pending rows too is what makes the limits hold
/// under concurrent calls: a call that is still in flight is already counted. Run it in a
/// transaction and commit it before the request is sent; a refusal records nothing.
pub async fn reserve_ai_call(
    connection: &mut PgConnection,
    call: &NewAiCall<'_>,
    limits: ReservationLimits,
    now: OffsetDateTime,
) -> Result<Reservation, StoreError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("ai_calls:{}", call.workspace_id.as_uuid()))
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    // A call still pending after the client's whole timeout is not in flight any more: its process
    // died. It is given up on but keeps its cost, since the provider may have billed it.
    sqlx::query(
        "UPDATE ai_calls SET outcome = 'abandoned' WHERE workspace_id = $1 AND outcome = 'pending' \n           AND created_at < now() - make_interval(secs => $2)",
    )
    .bind(call.workspace_id.as_uuid())
    .bind(limits.stale_after_secs)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    if calls_in_last_minute(&mut *connection, call.workspace_id, call.user_id).await?
        >= limits.rate_limit
    {
        return Ok(Reservation::RateLimited);
    }
    let spent = spent_this_month(&mut *connection, call.workspace_id, now).await?;
    if spent.saturating_add(call.estimated_cost_micros) > limits.budget_micros {
        return Ok(Reservation::BudgetExceeded);
    }
    let pending = NewAiCall {
        outcome: CallOutcome::Pending,
        ..call.clone()
    };
    record_ai_call(connection, &pending).await?;
    Ok(Reservation::Granted)
}

/// How a reserved call ended.
#[derive(Clone, Copy, Debug)]
pub struct FinishedAiCall {
    pub outcome: CallOutcome,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub estimated_cost_micros: i64,
    pub latency_ms: u32,
}

/// Finishes the `pending` row of a reserved call, replacing its worst-case cost with the actual
/// one. It fails when the row is not pending.
pub async fn finish_ai_call(
    connection: &mut PgConnection,
    id: Uuid,
    finished: &FinishedAiCall,
) -> Result<(), StoreError> {
    let clamp = |tokens: u32| i32::try_from(tokens).unwrap_or(i32::MAX);
    sqlx::query_scalar::<_, Uuid>(
        "UPDATE ai_calls SET input_tokens = $2, output_tokens = $3, estimated_cost_micros = $4, \n             latency_ms = $5, outcome = $6 WHERE id = $1 AND outcome = 'pending' RETURNING id",
    )
    .bind(id)
    .bind(clamp(finished.input_tokens))
    .bind(clamp(finished.output_tokens))
    .bind(finished.estimated_cost_micros.max(0))
    .bind(clamp(finished.latency_ms))
    .bind(finished.outcome.as_str())
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// The first instant of the UTC calendar month containing `now`.
pub fn month_start(now: OffsetDateTime) -> OffsetDateTime {
    now.to_offset(UtcOffset::UTC)
        .replace_day(1)
        .expect("the first day exists in every month")
        .replace_time(Time::MIDNIGHT)
}

/// What the workspace has spent since the first day of the month (UTC) containing `now`, in
/// micro-units.
pub async fn spent_this_month<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    now: OffsetDateTime,
) -> Result<i64, StoreError> {
    sqlx::query_scalar(
        "SELECT COALESCE(sum(estimated_cost_micros), 0)::bigint FROM ai_calls \
         WHERE workspace_id = $1 AND created_at >= $2",
    )
    .bind(workspace.as_uuid())
    .bind(month_start(now))
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)
}

/// How many calls one user (or, with `None`, calls made with an API key) made in the last
/// minute, by the database's clock.
pub async fn calls_in_last_minute<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    user: Option<UserId>,
) -> Result<i64, StoreError> {
    sqlx::query_scalar(
        "SELECT count(*) FROM ai_calls WHERE workspace_id = $1 \
           AND user_id IS NOT DISTINCT FROM $2 AND created_at > now() - interval '1 minute'",
    )
    .bind(workspace.as_uuid())
    .bind(user.map(|id| id.as_uuid()))
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeatureUsage {
    pub feature: String,
    pub calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_micros: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserUsage {
    pub user_id: Option<UserId>,
    pub calls: i64,
    pub cost_micros: i64,
}

/// A logged call, as shown on the usage page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AiCall {
    pub id: Uuid,
    pub user_id: Option<UserId>,
    pub feature: String,
    pub provider_id: String,
    pub model: String,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub estimated_cost_micros: i64,
    pub latency_ms: i32,
    pub outcome: String,
    pub created_at: OffsetDateTime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AiUsage {
    pub spent_micros: i64,
    pub by_feature: Vec<FeatureUsage>,
    pub by_user: Vec<UserUsage>,
    pub recent: Vec<AiCall>,
}

#[derive(FromRow)]
struct FeatureRow {
    feature: String,
    calls: i64,
    input_tokens: i64,
    output_tokens: i64,
    cost_micros: i64,
}

#[derive(FromRow)]
struct UserRow {
    user_id: Option<Uuid>,
    calls: i64,
    cost_micros: i64,
}

#[derive(FromRow)]
struct CallRow {
    id: Uuid,
    user_id: Option<Uuid>,
    feature: String,
    provider_id: String,
    model: String,
    input_tokens: i32,
    output_tokens: i32,
    estimated_cost_micros: i64,
    latency_ms: i32,
    outcome: String,
    created_at: OffsetDateTime,
}

/// The month's usage of a workspace, by feature and by user, with its `recent` newest calls.
pub async fn ai_usage(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    now: OffsetDateTime,
    recent: i64,
) -> Result<AiUsage, StoreError> {
    let start = month_start(now);
    let by_feature: Vec<FeatureRow> = sqlx::query_as(
        "SELECT feature, count(*) AS calls, \
                COALESCE(sum(input_tokens), 0)::bigint AS input_tokens, \
                COALESCE(sum(output_tokens), 0)::bigint AS output_tokens, \
                COALESCE(sum(estimated_cost_micros), 0)::bigint AS cost_micros \
         FROM ai_calls WHERE workspace_id = $1 AND created_at >= $2 \
         GROUP BY feature ORDER BY cost_micros DESC, feature",
    )
    .bind(workspace.as_uuid())
    .bind(start)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let by_user: Vec<UserRow> = sqlx::query_as(
        "SELECT user_id, count(*) AS calls, \
                COALESCE(sum(estimated_cost_micros), 0)::bigint AS cost_micros \
         FROM ai_calls WHERE workspace_id = $1 AND created_at >= $2 \
         GROUP BY user_id ORDER BY cost_micros DESC, user_id",
    )
    .bind(workspace.as_uuid())
    .bind(start)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let calls: Vec<CallRow> = sqlx::query_as(
        "SELECT id, user_id, feature, provider_id, model, input_tokens, output_tokens, \
                estimated_cost_micros, latency_ms, outcome, created_at \
         FROM ai_calls WHERE workspace_id = $1 ORDER BY created_at DESC, id DESC LIMIT $2",
    )
    .bind(workspace.as_uuid())
    .bind(recent)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    Ok(AiUsage {
        spent_micros: by_feature.iter().map(|row| row.cost_micros).sum(),
        by_feature: by_feature
            .into_iter()
            .map(|row| FeatureUsage {
                feature: row.feature,
                calls: row.calls,
                input_tokens: row.input_tokens,
                output_tokens: row.output_tokens,
                cost_micros: row.cost_micros,
            })
            .collect(),
        by_user: by_user
            .into_iter()
            .map(|row| UserUsage {
                user_id: row.user_id.map(UserId::from_uuid),
                calls: row.calls,
                cost_micros: row.cost_micros,
            })
            .collect(),
        recent: calls
            .into_iter()
            .map(|row| AiCall {
                id: row.id,
                user_id: row.user_id.map(UserId::from_uuid),
                feature: row.feature,
                provider_id: row.provider_id,
                model: row.model,
                input_tokens: row.input_tokens,
                output_tokens: row.output_tokens,
                estimated_cost_micros: row.estimated_cost_micros,
                latency_ms: row.latency_ms,
                outcome: row.outcome,
                created_at: row.created_at,
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::{Email, Slug, WorkspaceName};

    use super::*;
    use crate::create_workspace;

    async fn workspace(pool: &PgPool, name: &str) -> WorkspaceId {
        let name_parsed = WorkspaceName::parse(name).unwrap();
        let slug = Slug::parse(name).unwrap();
        create_workspace(pool, &name_parsed, &slug)
            .await
            .unwrap()
            .id
    }

    async fn record(
        pool: &PgPool,
        ws: WorkspaceId,
        user: Option<UserId>,
        feature: &str,
        cost: i64,
    ) -> Uuid {
        let id = Uuid::now_v7();
        let mut connection = pool.acquire().await.unwrap();
        record_ai_call(
            &mut connection,
            &NewAiCall {
                id,
                workspace_id: ws,
                user_id: user,
                feature,
                provider_id: "openai",
                model: "gpt-x",
                input_tokens: 100,
                output_tokens: 20,
                estimated_cost_micros: cost,
                latency_ms: 12,
                outcome: CallOutcome::Ok,
                draft_id: None,
            },
        )
        .await
        .unwrap();
        id
    }

    async fn backdate(pool: &PgPool, id: Uuid, at: OffsetDateTime) {
        sqlx::query("UPDATE ai_calls SET created_at = $2 WHERE id = $1")
            .bind(id)
            .bind(at)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn user(pool: &PgPool, email: &str) -> UserId {
        let email = Email::parse(email).unwrap();
        crate::create_user(pool, &email, Some("hash"), false)
            .await
            .unwrap()
            .id
    }

    #[test]
    fn the_month_starts_at_midnight_utc_on_the_first() {
        assert_eq!(
            month_start(datetime!(2026-03-17 23:59:59 +02:00)),
            datetime!(2026-03-01 00:00 UTC)
        );
        assert_eq!(
            month_start(datetime!(2026-12-31 23:00 -05:00)),
            datetime!(2027-01-01 00:00 UTC)
        );
    }

    #[test]
    fn outcomes_have_stable_names_matching_the_database_check() {
        let names: Vec<&str> = [
            CallOutcome::Pending,
            CallOutcome::Abandoned,
            CallOutcome::Ok,
            CallOutcome::HttpError,
            CallOutcome::Timeout,
            CallOutcome::EgressDenied,
            CallOutcome::Transport,
            CallOutcome::Malformed,
            CallOutcome::Capability,
        ]
        .map(CallOutcome::as_str)
        .to_vec();
        assert_eq!(
            names,
            [
                "pending",
                "abandoned",
                "ok",
                "http_error",
                "timeout",
                "egress_denied",
                "transport",
                "malformed",
                "capability"
            ]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_month_to_date_spend_sums_this_months_calls_of_one_workspace(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let theirs = record(&pool, two, None, "mapping", 9_999).await;
        backdate(&pool, theirs, datetime!(2026-05-03 08:00 UTC)).await;
        let april = record(&pool, one, None, "mapping", 1_000).await;
        backdate(&pool, april, datetime!(2026-04-30 23:59 UTC)).await;
        for cost in [300, 200] {
            let id = record(&pool, one, None, "mapping", cost).await;
            backdate(&pool, id, datetime!(2026-05-02 08:00 UTC)).await;
        }

        let may = datetime!(2026-05-15 12:00 UTC);
        assert_eq!(spent_this_month(&pool, one, may).await.unwrap(), 500);
        assert_eq!(spent_this_month(&pool, two, may).await.unwrap(), 9_999);
        let april_view = datetime!(2026-04-20 00:00 UTC);
        assert_eq!(
            spent_this_month(&pool, one, april_view).await.unwrap(),
            1_500
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_rate_limit_counts_one_users_calls_in_the_last_minute(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let ada = user(&pool, "ada@example.com").await;
        let bob = user(&pool, "bob@example.com").await;
        record(&pool, ws, Some(ada), "mapping", 1).await;
        record(&pool, ws, Some(ada), "mapping", 1).await;
        record(&pool, ws, Some(bob), "mapping", 1).await;
        record(&pool, ws, None, "mapping", 1).await;
        let stale = record(&pool, ws, Some(ada), "mapping", 1).await;
        sqlx::query("UPDATE ai_calls SET created_at = now() - interval '2 minutes' WHERE id = $1")
            .bind(stale)
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(calls_in_last_minute(&pool, ws, Some(ada)).await.unwrap(), 2);
        assert_eq!(calls_in_last_minute(&pool, ws, Some(bob)).await.unwrap(), 1);
        assert_eq!(calls_in_last_minute(&pool, ws, None).await.unwrap(), 1);
        let other = workspace(&pool, "two").await;
        assert_eq!(
            calls_in_last_minute(&pool, other, Some(ada)).await.unwrap(),
            0
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn usage_groups_the_month_by_feature_and_user_and_lists_recent_calls(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;
        let ada = user(&pool, "ada@example.com").await;
        record(&pool, ws, Some(ada), "mapping", 300).await;
        record(&pool, ws, Some(ada), "mapping", 100).await;
        record(&pool, ws, None, "manifest", 700).await;
        record(&pool, other, None, "mapping", 5).await;

        let mut connection = pool.acquire().await.unwrap();
        let usage = ai_usage(&mut connection, ws, OffsetDateTime::now_utc(), 2)
            .await
            .unwrap();

        assert_eq!(usage.spent_micros, 1_100);
        let features: Vec<(&str, i64, i64)> = usage
            .by_feature
            .iter()
            .map(|row| (row.feature.as_str(), row.calls, row.cost_micros))
            .collect();
        assert_eq!(features, [("manifest", 1, 700), ("mapping", 2, 400)]);
        assert_eq!(usage.by_feature[1].input_tokens, 200);
        assert_eq!(usage.by_user.len(), 2);
        assert_eq!(usage.by_user[0].user_id, None);
        assert_eq!(usage.by_user[1].user_id, Some(ada));
        assert_eq!(usage.recent.len(), 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_call_log_has_no_column_that_could_hold_content_and_refuses_unknown_outcomes(
        pool: PgPool,
    ) {
        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT column_name::text FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = 'ai_calls' ORDER BY 1",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            columns,
            [
                "created_at",
                "draft_id",
                "estimated_cost_micros",
                "feature",
                "id",
                "input_tokens",
                "latency_ms",
                "model",
                "outcome",
                "output_tokens",
                "provider_id",
                "user_id",
                "workspace_id"
            ]
        );
        let text_columns: Vec<String> = sqlx::query_scalar(
            "SELECT column_name::text FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = 'ai_calls' \
               AND data_type IN ('text', 'jsonb', 'json', 'bytea') ORDER BY 1",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(text_columns, ["feature", "model", "outcome", "provider_id"]);

        let ws = workspace(&pool, "one").await;
        let bad = sqlx::query(
            "INSERT INTO ai_calls (id, workspace_id, feature, provider_id, model, outcome) \
             VALUES ($1, $2, 'f', 'p', 'm', 'a prompt')",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .execute(&pool)
        .await;
        assert!(bad.is_err());
    }

    fn reservation(ws: WorkspaceId, user: Option<UserId>, cost: i64) -> NewAiCall<'static> {
        NewAiCall {
            id: Uuid::now_v7(),
            workspace_id: ws,
            user_id: user,
            feature: "mapping",
            provider_id: "openai",
            model: "gpt-x",
            input_tokens: 0,
            output_tokens: 0,
            estimated_cost_micros: cost,
            latency_ms: 0,
            outcome: CallOutcome::Ok,
            draft_id: None,
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_reservation_is_pending_and_counts_in_both_limits_until_it_is_finished(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let ada = user(&pool, "ada@example.com").await;
        let now = OffsetDateTime::now_utc();
        let reserve = |call: NewAiCall<'static>, rate: i64, budget: i64| {
            let pool = pool.clone();
            async move {
                let mut tx = pool.begin().await.unwrap();
                let limits = ReservationLimits {
                    rate_limit: rate,
                    budget_micros: budget,
                    stale_after_secs: 3_600.0,
                };
                let result = reserve_ai_call(&mut tx, &call, limits, now).await.unwrap();
                tx.commit().await.unwrap();
                result
            }
        };

        let first = reservation(ws, Some(ada), 400);
        assert_eq!(reserve(first.clone(), 2, 1_000).await, Reservation::Granted);
        let outcome: String = sqlx::query_scalar("SELECT outcome FROM ai_calls WHERE id = $1")
            .bind(first.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(outcome, "pending", "the caller's outcome is not trusted");
        assert_eq!(
            reserve(reservation(ws, Some(ada), 400), 2, 1_000).await,
            Reservation::Granted
        );
        // Two pending rows are 800 micro-units: under the budget, but at the rate limit.
        assert_eq!(
            reserve(reservation(ws, Some(ada), 1), 2, 1_000).await,
            Reservation::RateLimited
        );
        assert_eq!(
            reserve(reservation(ws, None, 400), 2, 800).await,
            Reservation::BudgetExceeded
        );
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM ai_calls")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 2, "a refusal records nothing");

        let mut connection = pool.acquire().await.unwrap();
        finish_ai_call(
            &mut connection,
            first.id,
            &FinishedAiCall {
                outcome: CallOutcome::Ok,
                input_tokens: 10,
                output_tokens: 5,
                estimated_cost_micros: 30,
                latency_ms: 7,
            },
        )
        .await
        .unwrap();
        assert_eq!(spent_this_month(&pool, ws, now).await.unwrap(), 430);
        let again = finish_ai_call(
            &mut connection,
            first.id,
            &FinishedAiCall {
                outcome: CallOutcome::Timeout,
                input_tokens: 0,
                output_tokens: 0,
                estimated_cost_micros: 0,
                latency_ms: 0,
            },
        )
        .await;
        assert!(again.is_err(), "only a pending row can be finished");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_old_pending_row_is_abandoned_with_its_cost_at_the_next_reservation(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;
        let now = OffsetDateTime::now_utc();
        let mut stale = Vec::new();
        for (workspace, age) in [(ws, "2 hours"), (ws, "10 seconds"), (other, "2 hours")] {
            let call = reservation(workspace, None, 500);
            let mut connection = pool.acquire().await.unwrap();
            record_ai_call(
                &mut connection,
                &NewAiCall {
                    outcome: CallOutcome::Pending,
                    ..call.clone()
                },
            )
            .await
            .unwrap();
            sqlx::query("UPDATE ai_calls SET created_at = now() - $2::interval WHERE id = $1")
                .bind(call.id)
                .bind(age)
                .execute(&pool)
                .await
                .unwrap();
            stale.push(call.id);
        }

        let mut tx = pool.begin().await.unwrap();
        let granted = reserve_ai_call(
            &mut tx,
            &reservation(ws, None, 100),
            ReservationLimits {
                rate_limit: 100,
                budget_micros: 10_000,
                stale_after_secs: 90.0,
            },
            now,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(granted, Reservation::Granted);
        let mut outcomes = Vec::new();
        for id in &stale {
            let row: (String, i64) =
                sqlx::query_as("SELECT outcome, estimated_cost_micros FROM ai_calls WHERE id = $1")
                    .bind(id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            outcomes.push(row);
        }
        assert_eq!(
            outcomes,
            [
                ("abandoned".to_owned(), 500),
                ("pending".to_owned(), 500),
                ("pending".to_owned(), 500),
            ],
            "only this workspace's old row, and its cost is kept"
        );
        assert_eq!(spent_this_month(&pool, ws, now).await.unwrap(), 1_100);
    }
}
