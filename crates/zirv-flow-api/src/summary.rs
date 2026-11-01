//! The workspace summary behind the Home figures: aggregate counts over the last few UTC days and
//! the figures of each published flow. Counts only, never a person's data. The figures are read
//! from the daily rollup that the `summary.rollup` job fills every five minutes, so a request
//! costs a few rollup rows and the figures are up to about seven minutes old.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use serde::Serialize;
use time::OffsetDateTime;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;
use zirv_flow_store::WorkspaceSummary;

use crate::AppState;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::problem::{ApiError, Problem};

const DEFAULT_DAYS: i64 = 7;
const MAX_DAYS: i64 = 30;

/// What reading events demands: the need of the event explorer routes (`rbac.rs` holds the row
/// and a test keeps the two equal).
#[cfg(test)]
const EVENTS_NEED: Need = Need::Read;

#[derive(Serialize, ToSchema)]
pub(crate) struct SummaryWindowBody {
    /// Start of the window, inclusive (UTC): midnight of the first day counted.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    from: OffsetDateTime,
    /// End of the window (UTC): the time of the request.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    to: OffsetDateTime,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct FlowSummaryBody {
    flow_id: Uuid,
    /// Enrollments in flight (active, waiting, held or failed) at the last rollup run.
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
    /// When the rollup last finished a run. The counts lag the data by up to about seven
    /// minutes (five between runs and a two minute commit margin); a workspace the rollup has
    /// not reached yet shows zeros and the time of the request.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    computed_at: OffsetDateTime,
}

/// The query string of the summary route. Parsed by hand so a bad value is a problem+json 400
/// naming the range; the struct also declares the parameter for the OpenAPI document.
#[derive(IntoParams)]
#[into_params(parameter_in = Query)]
struct SummaryQuery {
    /// Days in the window, 1 to 30; default 7. Whole UTC days, today included. The figures come
    /// from a rollup refreshed every five minutes and may lag by up to about seven minutes (see
    /// `computed_at`).
    #[param(value_type = u32, required = false, minimum = 1, maximum = 30, default = 7)]
    days: Option<String>,
}

impl SummaryQuery {
    fn from_query(query: &str) -> Self {
        let days = url::form_urlencoded::parse(query.as_bytes())
            .filter(|(key, _)| key == "days")
            .map(|(_, value)| value.into_owned())
            .last();
        Self { days }
    }
}

/// `days` from the query string: 1 to 30, default 7.
fn parse_days(query: &str) -> Result<i64, ApiError> {
    match SummaryQuery::from_query(query).days {
        None => Ok(DEFAULT_DAYS),
        Some(value) => value
            .parse::<i64>()
            .ok()
            .filter(|days| (1..=MAX_DAYS).contains(days))
            .ok_or_else(|| ApiError::bad_request("days must be a number from 1 to 30")),
    }
}

fn body(
    figures: &WorkspaceSummary,
    may_read_events: bool,
    now: OffsetDateTime,
) -> WorkspaceSummaryBody {
    WorkspaceSummaryBody {
        window: SummaryWindowBody {
            from: figures.from,
            to: now,
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
        computed_at: figures.computed_at.unwrap_or(now),
    }
}

/// What happened in the workspace over the last `days` UTC days (today included), for the Home
/// figures. Read from the daily rollup, not counted per request.
#[utoipa::path(
    get,
    operation_id = "get_workspace_summary",
    path = "/api/v1/workspaces/{ws}/summary",
    tag = "workspaces",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        SummaryQuery
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
    // Reading events demands `EVENTS_NEED`, the `Need::Read` just checked, so no role can lack it
    // today; the `rbac.rs` test below fails if the event routes ever ask for more.
    let may_read_events = true;
    let now = state.clock.now_in_process();
    let figures = zirv_flow_store::workspace_summary(&state.pool, workspace.id, days, now).await?;
    Ok(Json(body(&figures, may_read_events, now)))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::Value;
    use sqlx::PgPool;
    use time::Duration;
    use time::macros::datetime;
    use tower::ServiceExt;
    use zirv_flow_core::{Scope, Slug, WorkspaceId, WorkspaceName};
    use zirv_flow_store::{Clock, FlowFigures, ManualClock};

    use super::*;
    use crate::admin::{self, Context};
    use crate::rbac::{Class, Method, ROUTES};
    use crate::router;

    fn figures(events: i64, computed_at: Option<OffsetDateTime>) -> WorkspaceSummary {
        WorkspaceSummary {
            from: datetime!(2030-01-09 00:00 UTC),
            computed_at,
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
        let now = datetime!(2030-01-15 12:00 UTC);
        let value = serde_json::to_value(body(&figures(2_400_000, None), false, now)).unwrap();
        assert_eq!(value["events_received"], Value::Null);
        assert_eq!(value["messages_sent"], 6945);
        let value = serde_json::to_value(body(&figures(2_400_000, None), true, now)).unwrap();
        assert_eq!(value["events_received"], 2_400_000);
        assert_eq!(value["flows"][0]["people_in_flow"], 1284);
    }

    #[test]
    fn computed_at_is_the_rollups_and_the_request_time_only_before_its_first_run() {
        let now = datetime!(2030-01-15 12:00 UTC);
        let ran = datetime!(2030-01-15 11:55 UTC);

        let after = serde_json::to_value(body(&figures(1, Some(ran)), true, now)).unwrap();
        let before = serde_json::to_value(body(&figures(1, None), true, now)).unwrap();

        assert_eq!(after["computed_at"], "2030-01-15T11:55:00Z");
        assert_eq!(after["window"]["to"], "2030-01-15T12:00:00Z");
        assert_eq!(before["computed_at"], "2030-01-15T12:00:00Z");
    }

    #[test]
    fn events_are_read_with_the_need_of_the_event_explorer() {
        let explorer = ROUTES
            .iter()
            .find(|route| {
                route.method == Method::Post
                    && route.path == "/api/v1/workspaces/{ws}/events/explore/count"
            })
            .unwrap();

        assert_eq!(explorer.class, Class::Need(EVENTS_NEED));
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
    async fn the_route_validates_days_scopes_to_the_workspace_and_sums_the_rollup(pool: PgPool) {
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
        let now = datetime!(2030-01-15 12:00 UTC);
        let clock = ManualClock::new(now);
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
        people(ours, datetime!(2030-01-02 00:00 UTC)).await;
        people(theirs, datetime!(2030-01-14 00:00 UTC)).await;

        for bad in ["days=0", "days=31", "days=x"] {
            assert_eq!(
                call(&router, &format!("{uri}?{bad}"), &key).await.0,
                StatusCode::BAD_REQUEST
            );
        }
        // Before the first rollup run the figures are zero.
        let (status, empty) = call(&router, &uri, &key).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(empty["people_created"], 0);

        for workspace in [ours, theirs] {
            for _ in 0..10 {
                zirv_flow_store::refresh_workspace_summary(&pool, workspace, now)
                    .await
                    .unwrap();
            }
        }
        people(ours, datetime!(2030-01-14 06:00 UTC)).await;
        clock.advance(Duration::minutes(1));
        let (_, week) = call(&router, &uri, &key).await;
        let (_, month) = call(&router, &format!("{uri}?days=30"), &key).await;

        // The row written after the run is not shown until the next run.
        assert_eq!(week["people_created"], 1);
        assert_eq!(month["people_created"], 2);
        assert_eq!(week["events_received"], 0);
        assert_eq!(week["computed_at"], "2030-01-15T12:00:00Z");
        assert_eq!(week["flows"], serde_json::json!([]));
        let other = format!("/api/v1/workspaces/{}/summary", theirs.as_uuid());
        assert_eq!(call(&router, &other, &key).await.0, StatusCode::NOT_FOUND);
    }
}
