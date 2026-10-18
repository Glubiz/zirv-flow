//! Segments (architecture sections 6, 8 and 15): the definition AST schemas, saving versions
//! under the cycle check, and preview with data age.

pub(crate) mod crud;
pub(crate) mod explain;
pub(crate) mod members;
pub(crate) mod preview;
mod schema;

use std::time::Duration;

use zirv_flow_segments::{Catalog, Issue, TraitStorage};
use zirv_flow_store::{CatalogSnapshot, SegmentError, SegmentRef};

use crate::problem::{ApiError, SegmentIssueProblem, SegmentRefProblem};

pub(crate) use schema::{
    NODE as SEGMENT_NODE_SCHEMA, PROPERTY_FILTER as SEGMENT_FILTER_SCHEMA, SegmentSchemas,
};

/// How long a preview may run before PostgreSQL cancels it (architecture 8.3). It is fixed, with no
/// setting, and sits above the 15 s target of an aggregate preview (architecture 19).
pub(crate) const PREVIEW_TIMEOUT: Duration = Duration::from_secs(20);

/// The compiler's catalog and trait storage from the store's snapshot. Only slots in state
/// `ready` are in the storage, and they come from the same snapshot as the catalog.
pub(crate) fn catalog_of(snapshot: CatalogSnapshot) -> (Catalog, TraitStorage) {
    let storage = TraitStorage::with_slots(snapshot.slots);
    let catalog = Catalog {
        traits: snapshot.traits,
        events: snapshot.events,
        lists: snapshot.lists,
        segments: snapshot.segments,
        topics: snapshot.topics,
    };
    (catalog, storage)
}

/// The 422 `segment_invalid` problem for a definition's issues.
pub(crate) fn issues_error(issues: Vec<Issue>) -> ApiError {
    ApiError::segment_invalid(
        issues
            .into_iter()
            .map(|issue| SegmentIssueProblem {
                pointer: issue.pointer,
                code: issue.code.to_owned(),
            })
            .collect(),
    )
}

fn named(segments: Vec<SegmentRef>) -> Vec<SegmentRefProblem> {
    segments
        .into_iter()
        .map(|segment| SegmentRefProblem {
            id: segment.id,
            name: segment.name,
        })
        .collect()
}

/// The problem for a refused segment write.
pub(crate) fn segment_error(error: SegmentError) -> ApiError {
    match error {
        SegmentError::NotFound => ApiError::not_found(),
        SegmentError::Archived => ApiError::segment_archived(),
        SegmentError::Cycle(cycle) => ApiError::segment_cycle(named(cycle)),
        SegmentError::Referenced(by) => ApiError::segment_referenced(&named(by)),
        SegmentError::Store(error) => name_taken(error),
    }
}

/// A unique violation on a segment write is a name that is taken.
pub(crate) fn name_taken(error: zirv_flow_store::StoreError) -> ApiError {
    if error.is_conflict() {
        ApiError::new(
            axum::http::StatusCode::CONFLICT,
            "a segment with that name already exists",
        )
    } else {
        error.into()
    }
}
