//! Where a segment is used: the flows whose live or draft version names it, the other segments
//! whose current definition references it, and the per-segment numbers of the segment list (flows
//! using it, people gained over seven days). One rule, `zirv_flow_core::flow::segment_uses`,
//! decides what "uses" means; the store reads the versions and applies it.

use std::collections::{BTreeMap, BTreeSet};

use hashbrown::HashMap;
use sqlx::PgConnection;
use sqlx::prelude::FromRow;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::flow::{FlowGraph, FlowSettings, GRAPH_VERSION, SegmentUse, segment_uses};

use super::cycle::SegmentRef;
use crate::StoreError;

/// How many days of membership history the list's change covers.
const CHANGE_DAYS: i64 = 7;

/// Which copy of a flow uses a segment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlowStage {
    /// The published version the flow runs (state `published` or `paused`).
    Live,
    /// Only the editable draft names the segment.
    Draft,
}

/// A flow that uses a segment, once, with every way it does.
#[derive(Clone, Debug, PartialEq)]
pub struct SegmentFlowUse {
    pub flow_id: Uuid,
    pub name: String,
    /// The flow's lifecycle state.
    pub state: String,
    /// `Live` when the live version uses the segment, else `Draft`.
    pub stage: FlowStage,
    /// The live version number for `Live`, else none.
    pub version: Option<i32>,
    /// The uses in the version that `stage` names.
    pub uses: Vec<SegmentUse>,
}

/// Where a segment is used.
#[derive(Clone, Debug, PartialEq)]
pub struct SegmentWhereUsed {
    /// By flow name.
    pub flows: Vec<SegmentFlowUse>,
    /// The other live segments whose current version references it, by name.
    pub segments: Vec<SegmentRef>,
}

/// The numbers the segment list shows beside a segment.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SegmentListStats {
    /// Flows whose live version uses the segment, each once.
    pub used_by_flows: i64,
    /// People entered minus people exited over the last 7 days; none while the segment is less
    /// than 7 days old.
    pub change_7d: Option<i64>,
}

type Uses = BTreeMap<Uuid, BTreeSet<SegmentUse>>;

struct FlowUses {
    id: Uuid,
    name: String,
    state: String,
    version: Option<i32>,
    live: Uses,
    draft: Uses,
}

#[derive(FromRow)]
struct FlowRow {
    id: Uuid,
    name: String,
    state: String,
    current_version: Option<i32>,
    draft_graph: serde_json::Value,
    draft_settings: serde_json::Value,
    live_graph: Option<serde_json::Value>,
    live_settings: Option<serde_json::Value>,
}

/// A graph that does not parse (a draft may be half-built) is read as empty, so the settings still
/// count; settings that do not parse name nothing.
fn uses_of(graph: &serde_json::Value, settings: &serde_json::Value) -> Uses {
    let Ok(settings) = serde_json::from_value::<FlowSettings>(settings.clone()) else {
        return Uses::new();
    };
    let graph = serde_json::from_value::<FlowGraph>(graph.clone()).unwrap_or(FlowGraph {
        v: GRAPH_VERSION,
        nodes: Vec::new(),
        edges: Vec::new(),
    });
    segment_uses(&graph, &settings)
}

/// The non-archived flows of the workspace with what their live and draft versions use. One read.
async fn load_flow_uses(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<Vec<FlowUses>, StoreError> {
    let rows: Vec<FlowRow> = sqlx::query_as(
        "SELECT f.id, f.name, f.state, f.current_version, f.draft_graph, f.draft_settings, \
            v.graph AS live_graph, v.settings AS live_settings \
         FROM flows f \
         LEFT JOIN flow_versions v ON v.flow_id = f.id AND v.version = f.current_version \
            AND f.state IN ('published', 'paused') \
         WHERE f.workspace_id = $1 AND f.archived_at IS NULL \
         ORDER BY lower(f.name), f.id",
    )
    .bind(workspace.as_uuid())
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let live = match (&row.live_graph, &row.live_settings) {
                (Some(graph), Some(settings)) => uses_of(graph, settings),
                _ => Uses::new(),
            };
            FlowUses {
                draft: uses_of(&row.draft_graph, &row.draft_settings),
                id: row.id,
                name: row.name,
                state: row.state,
                version: row.current_version.filter(|_| row.live_graph.is_some()),
                live,
            }
        })
        .collect())
}

/// The flows whose live or draft version uses `segment` and the live segments whose current
/// version references it. An archived flow is not listed; everything is the workspace's own.
pub async fn segment_where_used(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    segment: Uuid,
) -> Result<SegmentWhereUsed, StoreError> {
    let flows = load_flow_uses(&mut *connection, workspace)
        .await?
        .into_iter()
        .filter_map(|flow| {
            let (stage, version, uses) = match (flow.live.get(&segment), flow.draft.get(&segment)) {
                (Some(live), _) => (FlowStage::Live, flow.version, live),
                (None, Some(draft)) => (FlowStage::Draft, None, draft),
                (None, None) => return None,
            };
            Some(SegmentFlowUse {
                flow_id: flow.id,
                name: flow.name,
                state: flow.state,
                stage,
                version,
                uses: uses.iter().copied().collect(),
            })
        })
        .collect();
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT s.id, s.name FROM segments s \
         JOIN segment_versions v ON v.segment_id = s.id AND v.version = s.current_version \
         WHERE s.workspace_id = $1 AND s.archived_at IS NULL AND s.id <> $2 \
            AND v.deps -> 'segments' @> jsonb_build_array($2::text) \
         ORDER BY lower(s.name), s.id",
    )
    .bind(workspace.as_uuid())
    .bind(segment)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(SegmentWhereUsed {
        flows,
        segments: rows
            .into_iter()
            .map(|(id, name)| SegmentRef { id, name })
            .collect(),
    })
}

/// The list numbers of `segments` (id and creation time): two reads for the whole list, one over
/// the flows and one over the membership history. `now` is the store clock's instant.
pub async fn segment_list_stats(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    segments: &[(Uuid, OffsetDateTime)],
    now: OffsetDateTime,
) -> Result<HashMap<Uuid, SegmentListStats>, StoreError> {
    let since = now - Duration::days(CHANGE_DAYS);
    let mut stats: HashMap<Uuid, SegmentListStats> = segments
        .iter()
        .map(|&(id, created_at)| {
            let stat = SegmentListStats {
                used_by_flows: 0,
                change_7d: (created_at <= since).then_some(0),
            };
            (id, stat)
        })
        .collect();
    for flow in load_flow_uses(&mut *connection, workspace).await? {
        for segment in flow.live.keys() {
            if let Some(stat) = stats.get_mut(segment) {
                stat.used_by_flows += 1;
            }
        }
    }
    let ids: Vec<Uuid> = segments.iter().map(|&(id, _)| id).collect();
    let rows: Vec<(Uuid, i64, i64)> = sqlx::query_as(
        "SELECT segment_id, count(*) FILTER (WHERE entered_at >= $3), \
            count(*) FILTER (WHERE exited_at >= $3) \
         FROM segment_members \
         WHERE workspace_id = $1 AND segment_id = ANY($2) \
            AND (entered_at >= $3 OR exited_at >= $3) \
         GROUP BY segment_id",
    )
    .bind(workspace.as_uuid())
    .bind(&ids)
    .bind(since)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    for (segment, entered, exited) in rows {
        if let Some(stat) = stats.get_mut(&segment)
            && stat.change_7d.is_some()
        {
            stat.change_7d = Some(entered - exited);
        }
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;

    use super::*;
    use crate::flows::tests_support as flows;
    use crate::segments::tests_support as support;
    use crate::{FlowAction, FlowPatch, update_flow_draft};

    const NOW: OffsetDateTime = datetime!(2030-03-01 12:00 UTC);

    async fn set_draft(
        pool: &PgPool,
        ws: WorkspaceId,
        flow: Uuid,
        graph: serde_json::Value,
        settings: serde_json::Value,
    ) {
        let mut connection = pool.acquire().await.unwrap();
        let patch = FlowPatch {
            graph: Some(graph),
            settings: Some(settings),
            ..FlowPatch::default()
        };
        update_flow_draft(&mut connection, ws, flow, &patch)
            .await
            .unwrap();
    }

    fn branch_on(segment: Uuid) -> serde_json::Value {
        json!({"v": 1, "nodes": [
            {"id": "b", "type": "branch", "conditions": [{"segment": segment}]},
            {"id": "x", "type": "exit"}], "edges": []})
    }

    fn trigger_on(segment: Uuid) -> serde_json::Value {
        json!({"trigger": {"type": "segment", "segment_id": segment, "on": "entered"},
               "reentry": {"mode": "once"}})
    }

    /// A published flow whose version uses `segment` as the trigger and in a condition.
    async fn live_flow(pool: &PgPool, ws: WorkspaceId, name: &str, segment: Uuid) -> Uuid {
        let flow = flows::create(pool, ws, name).await;
        set_draft(pool, ws, flow.id, branch_on(segment), trigger_on(segment)).await;
        flows::publish(pool, ws, flow.id).await.unwrap();
        flow.id
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_flow_using_the_segment_every_way_is_listed_once_with_each_use(pool: PgPool) {
        let ws = support::workspace(&pool, "one").await;
        let segment = support::segment(&pool, ws, "Pro", 60).await;
        let flow = live_flow(&pool, ws, "Welcome", segment.id).await;
        let mut connection = pool.acquire().await.unwrap();

        let used = segment_where_used(&mut connection, ws, segment.id)
            .await
            .unwrap();

        assert_eq!(used.flows.len(), 1);
        let found = &used.flows[0];
        assert_eq!(
            (found.flow_id, found.stage, found.version),
            (flow, FlowStage::Live, Some(1))
        );
        assert_eq!(found.uses, [SegmentUse::Trigger, SegmentUse::Condition]);
        assert!(used.segments.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn draft_and_live_are_told_apart(pool: PgPool) {
        let ws = support::workspace(&pool, "one").await;
        let segment = support::segment(&pool, ws, "Pro", 60).await;
        let other = support::segment(&pool, ws, "Other", 60).await;
        let live = live_flow(&pool, ws, "Live", segment.id).await;
        // The live flow's draft moves on to another segment; its live version still uses this one.
        set_draft(&pool, ws, live, branch_on(other.id), trigger_on(other.id)).await;
        let draft = flows::create(&pool, ws, "Draft").await;
        set_draft(
            &pool,
            ws,
            draft.id,
            branch_on(segment.id),
            flows::settings(),
        )
        .await;
        let mut connection = pool.acquire().await.unwrap();

        let used = segment_where_used(&mut connection, ws, segment.id)
            .await
            .unwrap();
        let other_used = segment_where_used(&mut connection, ws, other.id)
            .await
            .unwrap();

        let summary: Vec<_> = used
            .flows
            .iter()
            .map(|f| (f.name.as_str(), f.stage, f.version))
            .collect();
        assert_eq!(
            summary,
            [
                ("Draft", FlowStage::Draft, None),
                ("Live", FlowStage::Live, Some(1))
            ]
        );
        assert_eq!(other_used.flows.len(), 1);
        assert_eq!(other_used.flows[0].stage, FlowStage::Draft);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn archived_flows_and_other_workspaces_are_excluded(pool: PgPool) {
        let one = support::workspace(&pool, "one").await;
        let two = support::workspace(&pool, "two").await;
        let segment = support::segment(&pool, one, "Pro", 60).await;
        let archived = live_flow(&pool, one, "Archived", segment.id).await;
        flows::lifecycle(&pool, one, archived, FlowAction::Archive)
            .await
            .unwrap();
        live_flow(&pool, two, "Elsewhere", segment.id).await;
        let mut connection = pool.acquire().await.unwrap();

        let used = segment_where_used(&mut connection, one, segment.id)
            .await
            .unwrap();
        let stats = segment_list_stats(
            &mut connection,
            one,
            &[(segment.id, segment.created_at)],
            segment.created_at,
        )
        .await
        .unwrap();

        assert!(used.flows.is_empty());
        assert_eq!(stats[&segment.id].used_by_flows, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn segments_referencing_it_are_listed_and_archived_ones_are_not(pool: PgPool) {
        let ws = support::workspace(&pool, "one").await;
        let base = support::segment(&pool, ws, "Base", 60).await;
        support::segment_referencing(&pool, ws, "Zed", base.id).await;
        support::segment_referencing(&pool, ws, "Alpha", base.id).await;
        let gone = support::segment_referencing(&pool, ws, "Gone", base.id).await;
        sqlx::query("UPDATE segments SET archived_at = now() WHERE id = $1")
            .bind(gone.id)
            .execute(&pool)
            .await
            .unwrap();
        let mut connection = pool.acquire().await.unwrap();

        let used = segment_where_used(&mut connection, ws, base.id)
            .await
            .unwrap();

        let names: Vec<_> = used.segments.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "Zed"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn used_by_flows_counts_live_versions_only_and_each_flow_once(pool: PgPool) {
        let ws = support::workspace(&pool, "one").await;
        let segment = support::segment(&pool, ws, "Pro", 60).await;
        live_flow(&pool, ws, "One", segment.id).await;
        let paused = live_flow(&pool, ws, "Two", segment.id).await;
        flows::lifecycle(&pool, ws, paused, FlowAction::Pause)
            .await
            .unwrap();
        let draft = flows::create(&pool, ws, "Draft").await;
        set_draft(
            &pool,
            ws,
            draft.id,
            branch_on(segment.id),
            trigger_on(segment.id),
        )
        .await;
        let mut connection = pool.acquire().await.unwrap();

        let stats = segment_list_stats(
            &mut connection,
            ws,
            &[(segment.id, segment.created_at)],
            segment.created_at,
        )
        .await
        .unwrap();

        assert_eq!(stats[&segment.id].used_by_flows, 2);
    }

    /// A membership row that entered `entered` hours before [`NOW`] and, when given, left `exited`
    /// hours before it.
    async fn membership(
        pool: &PgPool,
        ws: WorkspaceId,
        segment: Uuid,
        entered: i64,
        exited: Option<i64>,
    ) {
        let email = format!("p{}@example.test", Uuid::now_v7());
        let person = support::person(pool, ws, &email, "pro").await;
        sqlx::query(
            "INSERT INTO segment_members (workspace_id, segment_id, person_id, version, \
                entered_at, entered_trace_id, entered_cause, exited_version, exited_at, \
                exited_trace_id, exited_cause) \
             VALUES ($1, $2, $3, 1, $4, gen_random_uuid(), 'initial', \
                CASE WHEN $5::timestamptz IS NULL THEN NULL ELSE 1 END, $5, \
                CASE WHEN $5::timestamptz IS NULL THEN NULL ELSE gen_random_uuid() END, \
                CASE WHEN $5::timestamptz IS NULL THEN NULL ELSE 'data_change' END)",
        )
        .bind(ws.as_uuid())
        .bind(segment)
        .bind(person)
        .bind(NOW - Duration::hours(entered))
        .bind(exited.map(|hours| NOW - Duration::hours(hours)))
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn change_7d_is_entered_minus_exited_with_the_window_edge_included(pool: PgPool) {
        let ws = support::workspace(&pool, "one").await;
        let segment = support::segment(&pool, ws, "Pro", 60).await;
        let created = NOW - Duration::days(30);
        let week = 7 * 24;
        membership(&pool, ws, segment.id, 1, None).await;
        membership(&pool, ws, segment.id, week, None).await;
        membership(&pool, ws, segment.id, week + 1, None).await;
        // Entered outside the window and left inside it, and once more on the edge.
        membership(&pool, ws, segment.id, week + 50, Some(2)).await;
        membership(&pool, ws, segment.id, week + 50, Some(week)).await;
        membership(&pool, ws, segment.id, week + 50, Some(week + 1)).await;
        let mut connection = pool.acquire().await.unwrap();

        let stats = segment_list_stats(&mut connection, ws, &[(segment.id, created)], NOW)
            .await
            .unwrap();

        // Entered: 1 h and the edge. Exited: 2 h and the edge.
        assert_eq!(stats[&segment.id].change_7d, Some(0));
        membership(&pool, ws, segment.id, 3, None).await;
        let stats = segment_list_stats(&mut connection, ws, &[(segment.id, created)], NOW)
            .await
            .unwrap();
        assert_eq!(stats[&segment.id].change_7d, Some(1));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn change_7d_is_null_for_a_segment_younger_than_seven_days(pool: PgPool) {
        let ws = support::workspace(&pool, "one").await;
        let young = support::segment(&pool, ws, "Young", 60).await;
        let edge = support::segment(&pool, ws, "Edge", 60).await;
        membership(&pool, ws, young.id, 1, None).await;
        membership(&pool, ws, edge.id, 1, None).await;
        let mut connection = pool.acquire().await.unwrap();

        let stats = segment_list_stats(
            &mut connection,
            ws,
            &[
                (young.id, NOW - Duration::days(6)),
                (edge.id, NOW - Duration::days(7)),
            ],
            NOW,
        )
        .await
        .unwrap();

        assert_eq!(stats[&young.id].change_7d, None);
        assert_eq!(stats[&edge.id].change_7d, Some(1));
    }
}
