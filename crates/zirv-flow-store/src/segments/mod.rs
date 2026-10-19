//! Segments with versions (architecture sections 6, 7.5 and 8): the catalog a definition is
//! checked against, saving versions under the cycle check, the preview queries with data age,
//! and the members with the full refresh that computes them.
//! This crate never sees the compiler: the API validates and compiles, and passes in the checked
//! content and the SQL.

mod catalog;
mod crud;
mod cycle;
mod data_age;
mod explain;
mod members;
mod preview;
mod reevaluate;
mod refresh;
mod schedule;

pub use catalog::{
    CatalogDetails, CatalogSnapshot, EventDetail, PropertyDetail, TopicDetail, TraitDetail,
    read_segment_catalog, segment_catalog, segment_catalog_details,
};
pub use crud::{
    CADENCE_MINUTES, DEFAULT_CADENCE_MINUTES, NewSegment, NewSegmentVersion, Segment, SegmentError,
    SegmentPatch, SegmentVersion, archive_segment, create_segment, find_segment,
    find_segment_version, list_segment_versions, list_segments, lock_segments, update_segment,
};
pub use cycle::SegmentRef;
pub use data_age::{DataAge, FeedingSource, segment_data_age};
pub use explain::{
    ClearFields, EXPLAIN_EVENT_CAP, ExplainConsent, ExplainEvent, ExplainEventScope, ExplainPerson,
    explain_clear_fields, load_explain_person,
};
pub use members::{
    HistoryCursor, HistoryRow, MemberRow, segment_history_page, segment_members_page,
};
pub(crate) use members::{close_on_merge, forget_people};
#[cfg(any(test, feature = "test-support"))]
pub use preview::run_segment_ids;
pub use preview::{
    SampleRow, SqlParam, SqlQuery, begin_segment_snapshot, run_person_batch, run_segment_preview,
    snapshot_now,
};
pub(crate) use reevaluate::lock_segments_of_people;
pub use reevaluate::{
    DirtyBatch, ReevaluateApply, apply_reevaluation, lock_reevaluation, reevaluation_order,
};
pub use refresh::{
    MembershipCause, REFRESH_JOB_KIND, REFRESH_LOCK_CLASS, RefreshApply, RefreshOutcome,
    RefreshRequest, RefreshTrigger, SegmentMembershipChange, SegmentMembershipChanges,
    apply_refresh, decision_cause_kind, enqueue_refresh, lock_segment_refresh, membership_cause,
    membership_trace,
};
pub use schedule::{reconcile_segment_schedules, segment_cron};

/// Fixtures for the tests of this module.
#[cfg(test)]
pub(crate) mod tests_support {
    use serde_json::json;
    use sqlx::PgPool;
    use uuid::Uuid;
    use zirv_flow_core::{Cause, Slug, TraceContext, WorkspaceId, WorkspaceName};

    use super::{NewSegment, NewSegmentVersion, Segment, create_segment, lock_segments};
    use crate::people::{ProfileOutcome, ProfileRecord, apply_profile_batch};
    use crate::{begin, commit, create_workspace};

    pub(crate) async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    async fn make(
        pool: &PgPool,
        ws: WorkspaceId,
        name: &str,
        cadence_minutes: i32,
        references: &[Uuid],
    ) -> Segment {
        let content = NewSegmentVersion {
            definition: json!({"v": 1}),
            normalized: json!({"v": 1}),
            deps: json!({"segments": references.iter().map(Uuid::to_string).collect::<Vec<_>>()}),
            time_sensitive: false,
        };
        let new = NewSegment {
            id: Uuid::now_v7(),
            name: name.to_owned(),
            description: String::new(),
            cadence_minutes,
        };
        let mut tx = begin(pool).await.unwrap();
        lock_segments(&mut tx, ws).await.unwrap();
        let (segment, _) = create_segment(
            &mut tx,
            ws,
            &new,
            &content,
            None,
            &TraceContext::root(Cause::api("t")),
        )
        .await
        .unwrap();
        commit(tx).await.unwrap();
        segment
    }

    /// A live segment with version 1 and no references.
    pub(crate) async fn segment(
        pool: &PgPool,
        ws: WorkspaceId,
        name: &str,
        cadence_minutes: i32,
    ) -> Segment {
        make(pool, ws, name, cadence_minutes, &[]).await
    }

    /// A live segment whose version 1 references `other`.
    pub(crate) async fn segment_referencing(
        pool: &PgPool,
        ws: WorkspaceId,
        name: &str,
        other: Uuid,
    ) -> Segment {
        make(pool, ws, name, 60, &[other]).await
    }

    /// A person with an email and the `plan` trait.
    pub(crate) async fn person(pool: &PgPool, ws: WorkspaceId, email: &str, plan: &str) -> Uuid {
        let record = ProfileRecord {
            source: "crm".into(),
            stream: "contacts".into(),
            record_key: email.into(),
            identifiers: vec![("email".into(), email.into())],
            traits: json!({"plan": plan}).as_object().cloned().unwrap(),
            observed_at: time::macros::datetime!(2026-01-01 00:00 UTC),
            trace: TraceContext::root(Cause::api("t")),
        };
        match &apply_profile_batch(pool, ws, &[record]).await.unwrap()[0] {
            ProfileOutcome::Applied { person_id, .. }
            | ProfileOutcome::Unchanged { person_id, .. } => *person_id,
            ProfileOutcome::Rejected { reason } => panic!("rejected: {reason}"),
        }
    }
}
