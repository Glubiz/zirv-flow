//! Where a segment is used: the flows whose live or draft version names it and the segments whose
//! current definition references it.

use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_store::{FlowStage, SegmentWhereUsed};

use crate::AppState;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::problem::{ApiError, Problem};

#[derive(Serialize, ToSchema)]
pub(crate) struct WhereUsedFlowBody {
    flow_id: Uuid,
    name: String,
    /// The flow's lifecycle state: `draft`, `published` or `paused`.
    state: String,
    /// `live` when the live version uses the segment, `draft` when only the draft does.
    #[schema(example = "live")]
    stage: &'static str,
    /// The live version number; null for `draft`.
    version: Option<i32>,
    /// How the version uses the segment: `trigger`, `filter`, `condition` or `goal`.
    uses: Vec<&'static str>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct WhereUsedSegmentBody {
    segment_id: Uuid,
    name: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct WhereUsedBody {
    /// By flow name; each flow once.
    flows: Vec<WhereUsedFlowBody>,
    /// The other live segments whose current definition references the segment, by name.
    segments: Vec<WhereUsedSegmentBody>,
}

impl From<SegmentWhereUsed> for WhereUsedBody {
    fn from(used: SegmentWhereUsed) -> Self {
        Self {
            flows: used
                .flows
                .into_iter()
                .map(|flow| WhereUsedFlowBody {
                    flow_id: flow.flow_id,
                    name: flow.name,
                    state: flow.state,
                    stage: match flow.stage {
                        FlowStage::Live => "live",
                        FlowStage::Draft => "draft",
                    },
                    version: flow.version,
                    uses: flow.uses.iter().map(|how| how.as_str()).collect(),
                })
                .collect(),
            segments: used
                .segments
                .into_iter()
                .map(|segment| WhereUsedSegmentBody {
                    segment_id: segment.id,
                    name: segment.name,
                })
                .collect(),
        }
    }
}

/// The flows whose live or draft version uses the segment, and the segments that reference it.
/// Archived flows are not listed.
#[utoipa::path(
    get,
    operation_id = "segment_where_used",
    path = "/api/v1/workspaces/{ws}/segments/{id}/where-used",
    tag = "segments",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Segment id")
    ),
    responses(
        (status = 200, description = "Where the segment is used", body = WhereUsedBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or segment, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn where_used(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<WhereUsedBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let id = path_id(&id)?;
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    zirv_flow_store::find_segment(&mut *tx, workspace.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let used = zirv_flow_store::segment_where_used(&mut tx, workspace.id, id).await?;
    Ok(Json(used.into()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zirv_flow_core::flow::SegmentUse;
    use zirv_flow_store::{SegmentFlowUse, SegmentRef};

    use super::*;

    #[test]
    fn the_body_names_the_stage_and_each_use_and_keeps_draft_without_a_version() {
        let flow = |name: &str, stage, version| SegmentFlowUse {
            flow_id: Uuid::nil(),
            name: name.to_owned(),
            state: "published".to_owned(),
            stage,
            version,
            uses: vec![SegmentUse::Trigger, SegmentUse::Goal],
        };
        let body = WhereUsedBody::from(SegmentWhereUsed {
            flows: vec![
                flow("Live", FlowStage::Live, Some(3)),
                flow("Draft", FlowStage::Draft, None),
            ],
            segments: vec![SegmentRef {
                id: Uuid::nil(),
                name: "Child".to_owned(),
            }],
        });

        let wire = serde_json::to_value(&body).unwrap();

        assert_eq!(wire["flows"][0]["stage"], "live");
        assert_eq!(wire["flows"][0]["version"], 3);
        assert_eq!(wire["flows"][0]["uses"], json!(["trigger", "goal"]));
        assert_eq!(wire["flows"][1]["stage"], "draft");
        assert_eq!(wire["flows"][1]["version"], serde_json::Value::Null);
        assert_eq!(wire["segments"][0]["name"], "Child");
    }
}
