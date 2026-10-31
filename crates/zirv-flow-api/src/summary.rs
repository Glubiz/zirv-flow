//! The workspace summary behind the Home figures: aggregate counts over the last few days and
//! the figures of each published flow. Counts only, never a person's data. A result is kept in
//! memory for [`CACHE_FOR`] per workspace and window, so a busy Home does not run the counts on
//! every load.

use std::sync::{Arc, Mutex, PoisonError};

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use hashbrown::HashMap;
use serde::Serialize;
use time::{Duration, OffsetDateTime};
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_store::WorkspaceSummary;

use crate::AppState;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::problem::{ApiError, Problem};

/// How long a computed summary is served again.
const CACHE_FOR: Duration = Duration::seconds(60);
const DEFAULT_DAYS: i64 = 7;
const MAX_DAYS: i64 = 30;

type CacheKey = (WorkspaceId, i64);

/// A computed summary and the window it covers.
#[derive(Clone)]
struct Computed {
    from: OffsetDateTime,
    to: OffsetDateTime,
    figures: WorkspaceSummary,
}

/// The summaries computed in the last [`CACHE_FOR`], by workspace and number of days. Per process.
#[derive(Clone, Default)]
pub(crate) struct SummaryCache {
    entries: Arc<Mutex<HashMap<CacheKey, Arc<Computed>>>>,
}

impl SummaryCache {
    fn fresh(&self, key: CacheKey, now: OffsetDateTime) -> Option<Arc<Computed>> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries
            .get(&key)
            .filter(|computed| now >= computed.to && now - computed.to < CACHE_FOR)
            .cloned()
    }

    fn store(&self, key: CacheKey, computed: Arc<Computed>, now: OffsetDateTime) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.retain(|_, kept| now >= kept.to && now - kept.to < CACHE_FOR);
        entries.insert(key, computed);
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SummaryWindowBody {
    /// Start of the window, inclusive (UTC).
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    from: OffsetDateTime,
    /// End of the window (UTC): when the figures were computed.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    to: OffsetDateTime,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct FlowSummaryBody {
    flow_id: Uuid,
    /// Enrollments in flight now.
    people_in_flow: i64,
    /// Messages of the flow sent in the window.
    sent: i64,
    /// Goal conversions of the flow in the window; null when the flow has no goal.
    goals_reached: Option<i64>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct WorkspaceSummaryBody {
    window: SummaryWindowBody,
    /// Messages that ended `sent` in the window; test sends excluded.
    messages_sent: i64,
    /// Goal conversions in the window.
    goals_reached: i64,
    /// People created in the window.
    people_created: i64,
    /// Events received in the window. Null for a caller who may not read events.
    events_received: Option<i64>,
    /// Published flows only.
    flows: Vec<FlowSummaryBody>,
    /// When the figures were computed; up to 60 seconds old.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    computed_at: OffsetDateTime,
}

/// `days` from the query string: 1 to 30, default 7.
fn parse_days(query: &str) -> Result<i64, ApiError> {
    let mut days = DEFAULT_DAYS;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if key == "days" {
            days = value
                .parse::<i64>()
                .ok()
                .filter(|days| (1..=MAX_DAYS).contains(days))
                .ok_or_else(|| ApiError::bad_request("days must be a number from 1 to 30"))?;
        }
    }
    Ok(days)
}

fn body(computed: &Computed, may_read_events: bool) -> WorkspaceSummaryBody {
    let figures = &computed.figures;
    WorkspaceSummaryBody {
        window: SummaryWindowBody {
            from: computed.from,
            to: computed.to,
        },
        messages_sent: figures.messages_sent,
        goals_reached: figures.goals_reached,
        people_created: figures.people_created,
        events_received: may_read_events.then_some(figures.events_received),
        flows: figures
            .flows
            .iter()
            .map(|flow| FlowSummaryBody {
                flow_id: flow.flow_id,
                people_in_flow: flow.people_in_flow,
                sent: flow.sent,
                goals_reached: flow.goals_reached,
            })
            .collect(),
        computed_at: computed.to,
    }
}

/// What happened in the workspace over the last `days` days, for the Home figures.
#[utoipa::path(
    get,
    operation_id = "get_workspace_summary",
    path = "/api/v1/workspaces/{ws}/summary",
    tag = "workspaces",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("days" = Option<u32>, Query, description = "Days in the window, 1 to 30; default 7")
    ),
    responses(
        (status = 200, description = "The figures", body = WorkspaceSummaryBody),
        (status = 400, description = "days is not a number from 1 to 30", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The read ran past its time limit (`code` is `search_timeout`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<WorkspaceSummaryBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let days = parse_days(&query.unwrap_or_default())?;
    let key = (workspace.id, days);
    let now = state.clock.now_in_process();
    let computed = match state.summaries.fresh(key, now) {
        Some(computed) => computed,
        None => {
            let figures = zirv_flow_store::workspace_summary(
                &state.pool,
                workspace.id,
                now - Duration::days(days),
                now,
            )
            .await?;
            let computed = Arc::new(Computed {
                from: now - Duration::days(days),
                to: now,
                figures,
            });
            state.summaries.store(key, computed.clone(), now);
            computed
        }
    };
    // Events are read with the same need as the summary itself (the explorer is a `read`
    // route), so every caller that gets here may see the count; the field is the seam for a
    // role that cannot.
    Ok(Json(body(&computed, true)))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::Value;
    use sqlx::PgPool;
    use time::macros::datetime;
    use tower::ServiceExt;
    use zirv_flow_core::{Scope, Slug, WorkspaceName};
    use zirv_flow_store::{Clock, FlowFigures, ManualClock};

    use super::*;
    use crate::admin::{self, Context};
    use crate::router;

    fn computed(events: i64) -> Computed {
        Computed {
            from: datetime!(2030-01-08 00:00 UTC),
            to: datetime!(2030-01-15 00:00 UTC),
            figures: WorkspaceSummary {
                messages_sent: 6945,
                goals_reached: 642,
                people_created: 1120,
                events_received: events,
                flows: vec![FlowFigures {
                    flow_id: Uuid::nil(),
                    people_in_flow: 1284,
                    sent: 3902,
                    goals_reached: Some(642),
                }],
            },
        }
    }

    #[test]
    fn days_default_to_seven_and_must_be_one_to_thirty() {
        assert_eq!(parse_days("").unwrap(), 7);
        assert_eq!(parse_days("days=1").unwrap(), 1);
        assert_eq!(parse_days("x=1&days=30").unwrap(), 30);
        for bad in [
            "days=0", "days=31", "days=-1", "days=abc", "days=", "days=1.5",
        ] {
            assert_eq!(
                parse_days(bad).unwrap_err().status_code(),
                StatusCode::BAD_REQUEST,
                "{bad}"
            );
        }
    }

    #[test]
    fn a_caller_who_may_not_read_events_gets_null_for_them() {
        let value = serde_json::to_value(body(&computed(2_400_000), false)).unwrap();
        assert_eq!(value["events_received"], Value::Null);
        assert_eq!(value["messages_sent"], 6945);
        let value = serde_json::to_value(body(&computed(2_400_000), true)).unwrap();
        assert_eq!(value["events_received"], 2_400_000);
        assert_eq!(value["flows"][0]["people_in_flow"], 1284);
    }

    #[test]
    fn the_cache_serves_for_sixty_seconds_per_workspace_and_window() {
        let cache = SummaryCache::default();
        let ws = WorkspaceId::new();
        let at = datetime!(2030-01-15 00:00 UTC);
        cache.store((ws, 7), Arc::new(computed(1)), at);

        assert!(cache.fresh((ws, 7), at + Duration::seconds(59)).is_some());
        assert!(cache.fresh((ws, 7), at + Duration::seconds(60)).is_none());
        assert!(cache.fresh((ws, 30), at).is_none());
        assert!(cache.fresh((WorkspaceId::new(), 7), at).is_none());
    }

    async fn call(router: &axum::Router, uri: &str, key: &str) -> (StatusCode, Value) {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::AUTHORIZATION, format!("Bearer {key}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_route_validates_days_scopes_to_the_workspace_and_serves_from_the_cache(
        pool: PgPool,
    ) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let make = |slug: &'static str| {
            let pool = pool.clone();
            async move {
                zirv_flow_store::create_workspace(
                    &pool,
                    &WorkspaceName::parse(slug).unwrap(),
                    &Slug::parse(slug).unwrap(),
                )
                .await
                .unwrap()
                .id
            }
        };
        let (ours, theirs) = (make("ours").await, make("theirs").await);
        let key = admin::create_api_key(&pool, &Context::system(), ours, "k", &[Scope::Read])
            .await
            .unwrap()
            .plaintext;
        let clock = ManualClock::new(datetime!(2030-01-15 12:00 UTC));
        let state = AppState::new(pool.clone(), Arc::new(String::new))
            .with_clock(Clock::Manual(clock.clone()));
        let router = router(state);
        let uri = format!("/api/v1/workspaces/{}/summary", ours.as_uuid());
        let people = |workspace: WorkspaceId, at: OffsetDateTime| {
            let pool = pool.clone();
            async move {
                sqlx::query(
                    "INSERT INTO people (id, workspace_id, created_at) VALUES ($1, $2, $3)",
                )
                .bind(Uuid::now_v7())
                .bind(workspace.as_uuid())
                .bind(at)
                .execute(&pool)
                .await
                .unwrap();
            }
        };
        people(ours, datetime!(2030-01-14 00:00 UTC)).await;
        people(theirs, datetime!(2030-01-14 00:00 UTC)).await;

        for bad in ["days=0", "days=31", "days=x"] {
            assert_eq!(
                call(&router, &format!("{uri}?{bad}"), &key).await.0,
                StatusCode::BAD_REQUEST
            );
        }
        let (status, first) = call(&router, &uri, &key).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(first["people_created"], 1);
        assert_eq!(first["events_received"], 0);
        assert_eq!(first["flows"], serde_json::json!([]));

        people(ours, datetime!(2030-01-14 06:00 UTC)).await;
        clock.advance(Duration::seconds(30));
        let (_, cached) = call(&router, &uri, &key).await;
        assert_eq!(cached["people_created"], 1);
        assert_eq!(cached["computed_at"], first["computed_at"]);
        clock.advance(Duration::seconds(31));
        let (_, fresh) = call(&router, &uri, &key).await;
        assert_eq!(fresh["people_created"], 2);

        let other = format!("/api/v1/workspaces/{}/summary", theirs.as_uuid());
        assert_eq!(call(&router, &other, &key).await.0, StatusCode::NOT_FOUND);
    }
}
