//! Promoting a hot trait to a typed slot column and taking it back (architecture sections 6 and
//! 21.1). Marking assigns the lowest free slot of the trait's type and enqueues the backfill;
//! the compiler reads the slot only once the backfill has made it `ready`. Unmarking retires the
//! slot at once and enqueues the clear.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Serialize;
use serde_json::json;
use utoipa::ToSchema;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::people::SlotColumn;
use zirv_flow_store::{
    Assigned, NewJob, PgPool, Retired, SlotError, assign_trait_slot, retire_trait_slot,
};

use crate::AppState;
use crate::admin::{self, AdminError, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::problem::{ApiError, Problem};

/// Where a trait's slot is in its life.
#[derive(Serialize, ToSchema)]
pub(crate) struct IndexBody {
    key: String,
    /// `backfilling`, `ready` or `retiring`.
    #[schema(example = "backfilling")]
    index_state: String,
}

/// What a mark or an unmark did.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Change {
    /// A job was enqueued.
    Started(&'static str),
    /// Nothing changed: the trait was already in that state.
    Unchanged(&'static str),
}

impl Change {
    fn state(&self) -> &'static str {
        match self {
            Self::Started(state) | Self::Unchanged(state) => state,
        }
    }
}

fn refused(error: SlotError) -> AdminError {
    match error {
        SlotError::NoSuchTrait => AdminError::NotFound("trait"),
        SlotError::Store(error) => AdminError::Store(error),
        // The remaining refusals carry a stable code, so they are mapped in `slot_problem`.
        other => AdminError::Slot(other),
    }
}

/// The problem for a refused slot change.
pub(crate) fn slot_problem(error: &SlotError) -> ApiError {
    match error {
        SlotError::Full { kind, capacity } => ApiError::new(
            StatusCode::CONFLICT,
            format!(
                "all {capacity} {} slots of the workspace are in use",
                kind.as_str()
            ),
        )
        .with_code("trait_slots_full"),
        SlotError::Retiring => ApiError::new(
            StatusCode::CONFLICT,
            "the trait's index is being removed; mark it again once that finishes",
        )
        .with_code("trait_index_retiring"),
        SlotError::NotIndexed => ApiError::new(StatusCode::CONFLICT, "the trait is not indexed")
            .with_code("trait_not_indexed"),
        SlotError::Archived => ApiError::unprocessable("an archived trait cannot be indexed"),
        SlotError::NotSlottable => ApiError::unprocessable("a string_list trait cannot be indexed"),
        SlotError::TypeLocked => ApiError::new(
            StatusCode::CONFLICT,
            "the type of an indexed trait cannot change; remove the index first",
        )
        .with_code("trait_indexed"),
        SlotError::NoSuchTrait => ApiError::not_found(),
        SlotError::Store(_) => ApiError::internal(),
    }
}

fn enqueue_job(
    kind: &'static str,
    workspace: WorkspaceId,
    key: &str,
    slot: SlotColumn,
    generation: i64,
) -> NewJob {
    let mut job = NewJob::new(kind, crate::trace::request_trace());
    job.workspace_id = Some(workspace);
    job.payload = json!({ "workspace_id": workspace, "key": key, "slot": slot.as_str(), "generation": generation });
    job
}

/// Assigns a slot to the trait, audits `trait.indexed` and enqueues the backfill, in one
/// transaction.
pub(crate) async fn index_trait(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    key: &str,
) -> Result<Change, AdminError> {
    let mut tx = zirv_flow_store::begin(pool).await?;
    let assigned = assign_trait_slot(&mut tx, workspace, key)
        .await
        .map_err(refused)?;
    let change = match assigned {
        Assigned::New(slot, generation) => {
            admin::audit(
                &mut tx,
                context,
                Some(workspace),
                "trait.indexed",
                None,
                json!({ "slot": slot.as_str() }),
            )
            .await?;
            let job = enqueue_job("traits.slot_backfill", workspace, key, slot, generation);
            zirv_flow_store::enqueue(&mut *tx, &job).await?;
            Change::Started("backfilling")
        }
        Assigned::Existing(assignment) => Change::Unchanged(assignment.state.as_str()),
    };
    zirv_flow_store::commit(tx).await?;
    Ok(change)
}

/// Retires the trait's slot, audits `trait.unindexed` and enqueues the clear, in one transaction.
pub(crate) async fn unindex_trait(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    key: &str,
) -> Result<Change, AdminError> {
    let mut tx = zirv_flow_store::begin(pool).await?;
    let retired = retire_trait_slot(&mut tx, workspace, key)
        .await
        .map_err(refused)?;
    let change = match retired {
        Retired::Now(slot, generation) => {
            admin::audit(
                &mut tx,
                context,
                Some(workspace),
                "trait.unindexed",
                None,
                json!({ "slot": slot.as_str() }),
            )
            .await?;
            let job = enqueue_job("traits.slot_clear", workspace, key, slot, generation);
            zirv_flow_store::enqueue(&mut *tx, &job).await?;
            Change::Started("retiring")
        }
        Retired::Already(_) => Change::Unchanged("retiring"),
    };
    zirv_flow_store::commit(tx).await?;
    Ok(change)
}

fn reply(key: String, change: &Change) -> (StatusCode, Json<IndexBody>) {
    let status = match change {
        Change::Started(_) => StatusCode::ACCEPTED,
        Change::Unchanged(_) => StatusCode::OK,
    };
    (
        status,
        Json(IndexBody {
            key,
            index_state: change.state().to_owned(),
        }),
    )
}

/// Marks a trait as hot: it is copied into a typed column, and segments read the column once
/// the backfill has finished. Admins only.
#[utoipa::path(
    post,
    operation_id = "index_trait",
    path = "/api/v1/workspaces/{ws}/traits/{key}/index",
    tag = "people",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("key" = String, Path, description = "Trait key")
    ),
    responses(
        (status = 202, description = "A slot was assigned and the backfill enqueued", body = IndexBody),
        (status = 200, description = "The trait already holds a slot", body = IndexBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or trait, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`trait_slots_full`: every slot of the type is in use; `trait_index_retiring`: the previous index is still being removed", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "A string_list or archived trait cannot be indexed", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn index(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, key)): Path<(String, String)>,
) -> Result<(StatusCode, Json<IndexBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let change = index_trait(&state.pool, &principal.context(), workspace.id, &key).await?;
    Ok(reply(key, &change))
}

/// Takes a trait back from its slot: segments read the trait from its stored value again at
/// once, and the column is cleared and freed in the background. Admins only.
#[utoipa::path(
    delete,
    operation_id = "unindex_trait",
    path = "/api/v1/workspaces/{ws}/traits/{key}/index",
    tag = "people",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("key" = String, Path, description = "Trait key")
    ),
    responses(
        (status = 202, description = "The slot is retiring and the clear enqueued", body = IndexBody),
        (status = 200, description = "The slot was already retiring", body = IndexBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or trait, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`trait_not_indexed`: the trait holds no slot", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn unindex(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, key)): Path<(String, String)>,
) -> Result<(StatusCode, Json<IndexBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let change = unindex_trait(&state.pool, &principal.context(), workspace.id, &key).await?;
    Ok(reply(key, &change))
}

#[cfg(test)]
mod tests {
    use zirv_flow_core::people::SlotKind;

    use super::*;

    #[test]
    fn a_full_type_is_a_409_naming_the_type_and_its_capacity() {
        let problem = slot_problem(&SlotError::Full {
            kind: SlotKind::Date,
            capacity: 4,
        });

        assert_eq!(problem.status_code(), StatusCode::CONFLICT);
        assert_eq!(problem.code(), Some("trait_slots_full"));
        assert!(problem.detail().contains("4 date slots"));
    }

    #[test]
    fn the_other_refusals_have_their_own_status_and_code() {
        let retiring = slot_problem(&SlotError::Retiring);
        let unindexed = slot_problem(&SlotError::NotIndexed);
        let list = slot_problem(&SlotError::NotSlottable);

        assert_eq!(retiring.status_code(), StatusCode::CONFLICT);
        assert_eq!(retiring.code(), Some("trait_index_retiring"));
        assert_eq!(unindexed.code(), Some("trait_not_indexed"));
        assert_eq!(list.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            slot_problem(&SlotError::NoSuchTrait).status_code(),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn a_new_change_is_202_and_an_unchanged_one_is_200() {
        let started = reply("plan".into(), &Change::Started("backfilling"));
        let same = reply("plan".into(), &Change::Unchanged("ready"));

        assert_eq!(started.0, StatusCode::ACCEPTED);
        assert_eq!(same.0, StatusCode::OK);
        assert_eq!(same.1.index_state, "ready");
    }
}
