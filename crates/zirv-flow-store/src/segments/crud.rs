//! Creating, saving, archiving and reading segments and their versions. Every function is
//! scoped to one workspace. A write runs in the caller's transaction, after
//! [`lock_segments`], together with the audit entry.

use std::fmt;

use serde_json::Value;
use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{TraceContext, UserId, WorkspaceId};

use super::cycle::{SegmentRef, check_no_cycle, references_of};
use crate::StoreError;

/// The refresh cadences a segment may have, in minutes (ruling 303ac6ac). The database checks the
/// same set. Turning a cadence into a schedule is not done here.
pub const CADENCE_MINUTES: [i32; 13] =
    [5, 10, 15, 30, 60, 120, 180, 240, 360, 480, 720, 1440, 10080];

/// The cadence of a segment that is given none.
pub const DEFAULT_CADENCE_MINUTES: i32 = 60;

/// A segment: its identity and settings. The definition is in its versions.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct Segment {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub current_version: i32,
    pub cadence_minutes: i32,
    pub archived_at: Option<OffsetDateTime>,
    pub created_by: Option<Uuid>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// One immutable version of a segment's definition.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct SegmentVersion {
    pub segment_id: Uuid,
    pub version: i32,
    /// As it was saved.
    pub definition: Value,
    /// `normalize(definition)`.
    pub normalized: Value,
    /// The compiler's dependency summary; its `segments` array lists the referenced segments.
    pub deps: Value,
    pub time_sensitive: bool,
    /// The trace of the request that saved the version.
    pub trace_id: Uuid,
    pub created_by: Option<Uuid>,
    pub created_at: OffsetDateTime,
}

macro_rules! segment_columns {
    () => {
        "id, name, description, current_version, cadence_minutes, archived_at, created_by, \
         created_at, updated_at"
    };
}

macro_rules! version_columns {
    () => {
        "segment_id, version, definition, normalized, deps, time_sensitive, trace_id, \
         created_by, created_at"
    };
}

/// The checked content of a version.
#[derive(Clone, Debug)]
pub struct NewSegmentVersion {
    pub definition: Value,
    pub normalized: Value,
    pub deps: Value,
    pub time_sensitive: bool,
}

/// A segment to create.
#[derive(Clone, Debug)]
pub struct NewSegment {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub cadence_minutes: i32,
}

/// The fields of a segment to change; `None` leaves one as it is. A `version` is saved as the
/// next version and becomes the current one.
#[derive(Clone, Debug, Default)]
pub struct SegmentPatch {
    pub name: Option<String>,
    pub description: Option<String>,
    pub cadence_minutes: Option<i32>,
    pub version: Option<NewSegmentVersion>,
}

/// Why a segment write was refused.
#[derive(Debug)]
pub enum SegmentError {
    /// No such segment in the workspace.
    NotFound,
    /// The segment is archived.
    Archived,
    /// The save would close a cycle; the segments in path order, the first repeated at the end.
    Cycle(Vec<SegmentRef>),
    /// Another live segment's current version references the segment.
    Referenced(Vec<SegmentRef>),
    /// A name already taken is [`StoreError::is_conflict`].
    Store(StoreError),
}

impl From<StoreError> for SegmentError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl fmt::Display for SegmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("no such segment"),
            Self::Archived => formatter.write_str("the segment is archived"),
            Self::Cycle(_) => formatter.write_str("the segments would reference each other"),
            Self::Referenced(_) => formatter.write_str("another segment references the segment"),
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for SegmentError {}

/// Takes the workspace's segments lock for the rest of the transaction. Take it before reading
/// the catalog or the reference graph for a write, so two saves cannot each miss the other's
/// edge.
pub async fn lock_segments(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<(), StoreError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('segments/' || $1::text, 0))")
        .bind(workspace.as_uuid())
        .execute(connection)
        .await
        .map_err(StoreError::query)?;
    Ok(())
}

async fn insert_version(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    segment: Uuid,
    version: i32,
    content: &NewSegmentVersion,
    created_by: Option<UserId>,
    trace: &TraceContext,
) -> Result<SegmentVersion, StoreError> {
    sqlx::query_as(concat!(
        "INSERT INTO segment_versions (workspace_id, segment_id, version, definition, normalized, \
            deps, time_sensitive, trace_id, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING ",
        version_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(segment)
    .bind(version)
    .bind(&content.definition)
    .bind(&content.normalized)
    .bind(&content.deps)
    .bind(content.time_sensitive)
    .bind(trace.trace_id.as_uuid())
    .bind(created_by.map(|user| user.as_uuid()))
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)
}

/// Creates a segment with its first version. A new segment has no incoming reference, so it
/// cannot close a cycle. A live name already taken, ignoring case, is a conflict.
pub async fn create_segment(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    new: &NewSegment,
    content: &NewSegmentVersion,
    created_by: Option<UserId>,
    trace: &TraceContext,
) -> Result<(Segment, SegmentVersion), StoreError> {
    let segment: Segment = sqlx::query_as(concat!(
        "INSERT INTO segments (id, workspace_id, name, description, current_version, \
            cadence_minutes, created_by) VALUES ($1, $2, $3, $4, 1, $5, $6) RETURNING ",
        segment_columns!()
    ))
    .bind(new.id)
    .bind(workspace.as_uuid())
    .bind(&new.name)
    .bind(&new.description)
    .bind(new.cadence_minutes)
    .bind(created_by.map(|user| user.as_uuid()))
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let version =
        insert_version(connection, workspace, new.id, 1, content, created_by, trace).await?;
    Ok((segment, version))
}

/// Changes the given fields. A new definition becomes version n + 1 and the current one, after
/// the reference graph is checked for a cycle.
pub async fn update_segment(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    patch: &SegmentPatch,
    updated_by: Option<UserId>,
    trace: &TraceContext,
) -> Result<(Segment, Option<SegmentVersion>), SegmentError> {
    let held = lock_segment_row(connection, workspace, id).await?;
    if held.archived_at.is_some() {
        return Err(SegmentError::Archived);
    }
    let mut saved = None;
    let mut current = held.current_version;
    if let Some(content) = &patch.version {
        if let Some(cycle) =
            check_no_cycle(connection, workspace, id, &references_of(&content.deps)).await?
        {
            return Err(SegmentError::Cycle(cycle));
        }
        current += 1;
        saved = Some(
            insert_version(
                connection, workspace, id, current, content, updated_by, trace,
            )
            .await?,
        );
    }
    let segment: Segment = sqlx::query_as(concat!(
        "UPDATE segments SET name = COALESCE($3, name), description = COALESCE($4, description), \
            cadence_minutes = COALESCE($5, cadence_minutes), current_version = $6, \
            updated_at = now() WHERE workspace_id = $1 AND id = $2 RETURNING ",
        segment_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(patch.name.as_deref())
    .bind(patch.description.as_deref())
    .bind(patch.cadence_minutes)
    .bind(current)
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    Ok((segment, saved))
}

async fn lock_segment_row(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Segment, SegmentError> {
    sqlx::query_as(concat!(
        "SELECT ",
        segment_columns!(),
        " FROM segments WHERE workspace_id = $1 AND id = $2 FOR UPDATE"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?
    .ok_or(SegmentError::NotFound)
}

/// Archives a segment, unless another live segment's current version references it. Nothing is
/// deleted: versions stay readable.
pub async fn archive_segment(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Segment, SegmentError> {
    let held = lock_segment_row(connection, workspace, id).await?;
    if held.archived_at.is_some() {
        return Err(SegmentError::Archived);
    }
    let referencing: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT s.id, s.name FROM segments s \
         JOIN segment_versions v ON v.segment_id = s.id AND v.version = s.current_version \
         WHERE s.workspace_id = $1 AND s.archived_at IS NULL AND s.id <> $2 \
           AND v.deps -> 'segments' ? $3 ORDER BY lower(s.name), s.id",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(id.to_string())
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    if !referencing.is_empty() {
        return Err(SegmentError::Referenced(
            referencing
                .into_iter()
                .map(|(id, name)| SegmentRef { id, name })
                .collect(),
        ));
    }
    sqlx::query_as(concat!(
        "UPDATE segments SET archived_at = now(), updated_at = now() \
         WHERE workspace_id = $1 AND id = $2 RETURNING ",
        segment_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_one(connection)
    .await
    .map_err(|error| SegmentError::Store(StoreError::query(error)))
}

/// The workspace's segments by name; archived ones only when asked for.
pub async fn list_segments<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    include_archived: bool,
) -> Result<Vec<Segment>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        segment_columns!(),
        " FROM segments WHERE workspace_id = $1 AND ($2 OR archived_at IS NULL) \
          ORDER BY lower(name), id"
    ))
    .bind(workspace.as_uuid())
    .bind(include_archived)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

pub async fn find_segment<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<Segment>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        segment_columns!(),
        " FROM segments WHERE workspace_id = $1 AND id = $2"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// One version of a segment of the workspace.
pub async fn find_segment_version<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
    version: i32,
) -> Result<Option<SegmentVersion>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        version_columns!(),
        " FROM segment_versions WHERE workspace_id = $1 AND segment_id = $2 AND version = $3"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(version)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// The versions of a segment of the workspace, newest first.
pub async fn list_segment_versions<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Vec<SegmentVersion>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        version_columns!(),
        " FROM segment_versions WHERE workspace_id = $1 AND segment_id = $2 ORDER BY version DESC"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use zirv_flow_core::{Cause, Slug, WorkspaceName};

    use super::*;
    use crate::create_workspace;

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    fn trace() -> TraceContext {
        TraceContext::root(Cause::api("t"))
    }

    fn content(references: &[Uuid]) -> NewSegmentVersion {
        NewSegmentVersion {
            definition: json!({"v": 1}),
            normalized: json!({"v": 1}),
            deps: json!({"segments": references.iter().map(Uuid::to_string).collect::<Vec<_>>()}),
            time_sensitive: false,
        }
    }

    fn new_segment(name: &str, cadence_minutes: i32) -> NewSegment {
        NewSegment {
            id: Uuid::now_v7(),
            name: name.into(),
            description: String::new(),
            cadence_minutes,
        }
    }

    async fn create(pool: &PgPool, ws: WorkspaceId, name: &str, references: &[Uuid]) -> Segment {
        let mut tx = crate::begin(pool).await.unwrap();
        lock_segments(&mut tx, ws).await.unwrap();
        let new = new_segment(name, DEFAULT_CADENCE_MINUTES);
        let (segment, _) = create_segment(&mut tx, ws, &new, &content(references), None, &trace())
            .await
            .unwrap();
        crate::commit(tx).await.unwrap();
        segment
    }

    async fn save_traced(
        pool: &PgPool,
        ws: WorkspaceId,
        id: Uuid,
        references: &[Uuid],
        trace: &TraceContext,
    ) -> Result<(Segment, Option<SegmentVersion>), SegmentError> {
        let mut tx = crate::begin(pool).await.unwrap();
        lock_segments(&mut tx, ws).await.unwrap();
        let patch = SegmentPatch {
            version: Some(content(references)),
            ..SegmentPatch::default()
        };
        let saved = update_segment(&mut tx, ws, id, &patch, None, trace).await?;
        crate::commit(tx).await?;
        Ok(saved)
    }

    async fn save(
        pool: &PgPool,
        ws: WorkspaceId,
        id: Uuid,
        references: &[Uuid],
    ) -> Result<(Segment, Option<SegmentVersion>), SegmentError> {
        save_traced(pool, ws, id, references, &trace()).await
    }

    fn names(cycle: &[SegmentRef]) -> Vec<&str> {
        cycle.iter().map(|segment| segment.name.as_str()).collect()
    }

    #[test]
    fn the_cadences_are_the_ruled_set_and_the_default_is_one_of_them() {
        assert_eq!(CADENCE_MINUTES.len(), 13);
        assert!(CADENCE_MINUTES.contains(&DEFAULT_CADENCE_MINUTES));
        assert!(CADENCE_MINUTES.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(!CADENCE_MINUTES.contains(&7));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn each_save_makes_the_next_version_current_and_keeps_the_old_ones(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let segment = create(&pool, ws, "VIPs", &[]).await;
        assert_eq!(segment.current_version, 1);
        let request = trace();

        let (second, saved) = save_traced(&pool, ws, segment.id, &[], &request)
            .await
            .unwrap();
        let (third, _) = save(&pool, ws, segment.id, &[]).await.unwrap();

        let saved = saved.unwrap();
        assert_eq!((second.current_version, saved.version), (2, 2));
        assert_eq!(saved.trace_id, request.trace_id.as_uuid());
        assert_eq!(third.current_version, 3);
        let versions: Vec<i32> = list_segment_versions(&pool, ws, segment.id)
            .await
            .unwrap()
            .into_iter()
            .map(|version| version.version)
            .collect();
        assert_eq!(versions, [3, 2, 1]);
        let stored = find_segment_version(&pool, ws, segment.id, 2)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.trace_id, request.trace_id.as_uuid());
        let mut connection = pool.acquire().await.unwrap();
        let (renamed, version) = update_segment(
            &mut connection,
            ws,
            segment.id,
            &SegmentPatch {
                name: Some("Renamed".into()),
                ..SegmentPatch::default()
            },
            None,
            &trace(),
        )
        .await
        .unwrap();
        assert_eq!(
            (renamed.name.as_str(), renamed.current_version, version),
            ("Renamed", 3, None)
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_version_cannot_be_changed_and_the_current_one_must_exist(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let segment = create(&pool, ws, "A", &[]).await;

        let edit =
            sqlx::query("UPDATE segment_versions SET definition = '{}' WHERE segment_id = $1")
                .bind(segment.id)
                .execute(&pool)
                .await;
        let dangling = sqlx::query("UPDATE segments SET current_version = 9 WHERE id = $1")
            .bind(segment.id)
            .execute(&pool)
            .await;

        assert!(edit.is_err());
        assert!(dangling.is_err());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_self_reference_a_direct_and_a_transitive_cycle_are_refused_with_the_path(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let a = create(&pool, ws, "A", &[]).await;
        let b = create(&pool, ws, "B", &[a.id]).await;
        let c = create(&pool, ws, "C", &[b.id]).await;

        let own = save(&pool, ws, a.id, &[a.id]).await.unwrap_err();
        let direct = save(&pool, ws, a.id, &[b.id]).await.unwrap_err();
        let transitive = save(&pool, ws, a.id, &[c.id]).await.unwrap_err();

        let SegmentError::Cycle(own) = own else {
            panic!("{own}")
        };
        let SegmentError::Cycle(direct) = direct else {
            panic!("{direct}")
        };
        let SegmentError::Cycle(transitive) = transitive else {
            panic!("{transitive}")
        };
        assert_eq!(names(&own), ["A", "A"]);
        assert_eq!(names(&direct), ["A", "B", "A"]);
        assert_eq!(names(&transitive), ["A", "C", "B", "A"]);
        let unchanged = find_segment(&pool, ws, a.id).await.unwrap().unwrap();
        assert_eq!(unchanged.current_version, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_cycle_made_by_a_later_edit_is_refused(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let a = create(&pool, ws, "A", &[]).await;
        let b = create(&pool, ws, "B", &[]).await;
        save(&pool, ws, a.id, &[b.id]).await.unwrap();

        let error = save(&pool, ws, b.id, &[a.id]).await.unwrap_err();

        let SegmentError::Cycle(cycle) = error else {
            panic!("{error}")
        };
        assert_eq!(names(&cycle), ["B", "A", "B"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn two_concurrent_saves_that_would_form_a_cycle_leave_exactly_one(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let a = create(&pool, ws, "A", &[]).await;
        let b = create(&pool, ws, "B", &[]).await;

        let (to_b, to_a) = ([b.id], [a.id]);
        let (first, second) =
            tokio::join!(save(&pool, ws, a.id, &to_b), save(&pool, ws, b.id, &to_a));

        let succeeded = [first.is_ok(), second.is_ok()]
            .into_iter()
            .filter(|ok| *ok)
            .count();
        assert_eq!(succeeded, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_referenced_segment_cannot_be_archived_until_the_reference_is_gone(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let a = create(&pool, ws, "A", &[]).await;
        let b = create(&pool, ws, "B", &[a.id]).await;
        let mut connection = pool.acquire().await.unwrap();

        let refused = archive_segment(&mut connection, ws, a.id)
            .await
            .unwrap_err();
        save(&pool, ws, b.id, &[]).await.unwrap();
        let archived = archive_segment(&mut connection, ws, a.id).await.unwrap();
        let again = archive_segment(&mut connection, ws, a.id)
            .await
            .unwrap_err();

        let SegmentError::Referenced(by) = refused else {
            panic!("{refused}")
        };
        assert_eq!(names(&by), ["B"]);
        assert!(archived.archived_at.is_some());
        assert!(matches!(again, SegmentError::Archived));
        let live: Vec<String> = list_segments(&pool, ws, false)
            .await
            .unwrap()
            .into_iter()
            .map(|segment| segment.name)
            .collect();
        assert_eq!(live, ["B"]);
        assert_eq!(list_segments(&pool, ws, true).await.unwrap().len(), 2);
        let edit = save(&pool, ws, a.id, &[]).await.unwrap_err();
        assert!(matches!(edit, SegmentError::Archived));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn names_are_unique_among_live_segments_and_workspaces_are_isolated(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let first = create(&pool, one, "VIPs", &[]).await;
        let mut tx = crate::begin(&pool).await.unwrap();

        let duplicate = create_segment(
            &mut tx,
            one,
            &new_segment("vips", 60),
            &content(&[]),
            None,
            &trace(),
        )
        .await
        .unwrap_err();
        drop(tx);

        assert!(duplicate.is_conflict());
        create(&pool, two, "VIPs", &[]).await;
        assert_eq!(find_segment(&pool, two, first.id).await.unwrap(), None);
        assert!(
            find_segment_version(&pool, two, first.id, 1)
                .await
                .unwrap()
                .is_none()
        );
        let foreign = save(&pool, two, first.id, &[]).await.unwrap_err();
        assert!(matches!(foreign, SegmentError::NotFound));
        let mut connection = pool.acquire().await.unwrap();
        let archive = archive_segment(&mut connection, two, first.id)
            .await
            .unwrap_err();
        assert!(matches!(archive, SegmentError::NotFound));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_cadence_outside_the_set_is_refused_by_the_database(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let mut tx = crate::begin(&pool).await.unwrap();

        let refused = create_segment(
            &mut tx,
            ws,
            &new_segment("A", 7),
            &content(&[]),
            None,
            &trace(),
        )
        .await;

        assert!(refused.is_err());
    }
}
