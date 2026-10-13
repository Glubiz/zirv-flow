//! Identifier-level suppressions (architecture sections 6 and 16.3). A suppression is keyed
//! by the keyed hash of an identifier; the value is never stored, and the rows are not
//! person-scoped, so they outlive the person.

use sqlx::prelude::FromRow;
use sqlx::{PgExecutor, QueryBuilder};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::consent::SuppressionReason;

use crate::StoreError;

/// The identifier kinds a suppression can name.
pub const SUPPRESSION_KINDS: [&str; 2] = ["email", "phone"];

/// A suppression as stored.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Suppression {
    /// An opaque handle for paging; unlike the hash it is safe to hand out.
    pub id: Uuid,
    pub identifier_hash: [u8; 32],
    pub kind: String,
    pub reason: SuppressionReason,
    pub source: String,
    pub note: Option<String>,
    pub created_at: OffsetDateTime,
    pub created_by: Option<Uuid>,
}

/// A suppression to add.
#[derive(Clone, Debug)]
pub struct NewSuppression<'a> {
    pub identifier_hash: [u8; 32],
    pub kind: &'a str,
    pub reason: SuppressionReason,
    pub source: &'a str,
    /// At most 500 characters, and never the address itself.
    pub note: Option<&'a str>,
    pub created_by: Option<Uuid>,
}

/// One suppression of a batch.
#[derive(Clone, Copy, Debug)]
pub struct BatchEntry<'a> {
    pub identifier_hash: [u8; 32],
    pub kind: &'a str,
    pub note: Option<&'a str>,
}

/// What [`add_suppression`] did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddOutcome {
    Added,
    /// The hash was suppressed already and keeps its first reason.
    Existing,
    /// The hash was suppressed for another reason; `erased` replaced it.
    Upgraded,
}

/// What [`remove_suppression`] did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoveOutcome {
    Removed,
    NotFound,
    /// The suppression is `erased`, which is permanent.
    Erased,
}

#[derive(FromRow)]
struct SuppressionRow {
    id: Uuid,
    identifier_hash: Vec<u8>,
    kind: String,
    reason: String,
    source: String,
    note: Option<String>,
    created_at: OffsetDateTime,
    created_by: Option<Uuid>,
}

fn corrupt(what: &'static str) -> StoreError {
    StoreError::query(sqlx::Error::Decode(what.into()))
}

impl SuppressionRow {
    fn into_suppression(self) -> Result<Suppression, StoreError> {
        Ok(Suppression {
            id: self.id,
            identifier_hash: self
                .identifier_hash
                .try_into()
                .map_err(|_| corrupt("a suppression hash is not 32 bytes"))?,
            kind: self.kind,
            reason: self.reason.parse().map_err(|_| corrupt("unknown reason"))?,
            source: self.source,
            note: self.note,
            created_at: self.created_at,
            created_by: self.created_by,
        })
    }
}

/// Suppresses a hash. Adding one that exists keeps the row and its first reason, except that
/// `erased` replaces any other reason.
pub async fn add_suppression<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    new: &NewSuppression<'_>,
) -> Result<AddOutcome, StoreError> {
    // The old reason is read in the same statement that decides, so the answer is the
    // state the statement acted on.
    let (inserted, upgraded): (bool, bool) = sqlx::query_as(
        "WITH prior AS (\
            SELECT reason FROM suppressions WHERE workspace_id = $1 AND identifier_hash = $2), \
         written AS (\
            INSERT INTO suppressions \
                (workspace_id, identifier_hash, kind, reason, source, note, created_by, id) \
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
            ON CONFLICT (workspace_id, identifier_hash) DO UPDATE \
                SET reason = EXCLUDED.reason, source = EXCLUDED.source \
                WHERE EXCLUDED.reason = 'erased' AND suppressions.reason <> 'erased' \
            RETURNING 1) \
         SELECT NOT EXISTS (SELECT 1 FROM prior) AND EXISTS (SELECT 1 FROM written), \
                EXISTS (SELECT 1 FROM prior) AND EXISTS (SELECT 1 FROM written)",
    )
    .bind(workspace.as_uuid())
    .bind(new.identifier_hash.as_slice())
    .bind(new.kind)
    .bind(new.reason.as_str())
    .bind(new.source)
    .bind(new.note)
    .bind(new.created_by)
    .bind(Uuid::now_v7())
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(match (inserted, upgraded) {
        (true, _) => AddOutcome::Added,
        (_, true) => AddOutcome::Upgraded,
        _ => AddOutcome::Existing,
    })
}

/// Suppresses many hashes of one `reason` and `source` in one statement per chunk; hashes
/// already suppressed are left alone. Returns how many were added.
pub async fn add_suppressions<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    entries: &[BatchEntry<'_>],
    reason: SuppressionReason,
    source: &str,
    created_by: Option<Uuid>,
) -> Result<u64, StoreError> {
    if entries.is_empty() {
        return Ok(0);
    }
    let hashes: Vec<&[u8]> = entries
        .iter()
        .map(|entry| entry.identifier_hash.as_slice())
        .collect();
    let kinds: Vec<&str> = entries.iter().map(|entry| entry.kind).collect();
    let notes: Vec<Option<&str>> = entries.iter().map(|entry| entry.note).collect();
    let ids: Vec<Uuid> = entries.iter().map(|_| Uuid::now_v7()).collect();
    let added = sqlx::query(
        "INSERT INTO suppressions \
            (workspace_id, identifier_hash, kind, reason, source, note, created_by, id) \
         SELECT $1, hash, kind, $4, $5, note, $6, id \
         FROM unnest($2::bytea[], $3::text[], $7::text[], $8::uuid[]) AS t(hash, kind, note, id) \
         ON CONFLICT (workspace_id, identifier_hash) DO NOTHING",
    )
    .bind(workspace.as_uuid())
    .bind(hashes)
    .bind(kinds)
    .bind(reason.as_str())
    .bind(source)
    .bind(created_by)
    .bind(notes)
    .bind(ids)
    .execute(executor)
    .await
    .map_err(StoreError::query)?
    .rows_affected();
    Ok(added)
}

/// How many of `hashes` are suppressed already.
pub async fn count_suppressed<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    hashes: &[[u8; 32]],
) -> Result<u64, StoreError> {
    let hashes: Vec<&[u8]> = hashes.iter().map(|hash| hash.as_slice()).collect();
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM suppressions \
         WHERE workspace_id = $1 AND identifier_hash = ANY($2::bytea[])",
    )
    .bind(workspace.as_uuid())
    .bind(hashes)
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(count.unsigned_abs())
}

/// Lifts a suppression. An `erased` one is never lifted.
pub async fn remove_suppression<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    identifier_hash: &[u8; 32],
) -> Result<RemoveOutcome, StoreError> {
    let (removed, erased): (i64, bool) = sqlx::query_as(
        "WITH removed AS (\
            DELETE FROM suppressions \
            WHERE workspace_id = $1 AND identifier_hash = $2 AND reason <> 'erased' \
            RETURNING 1) \
         SELECT (SELECT count(*) FROM removed), \
                EXISTS (SELECT 1 FROM suppressions \
                        WHERE workspace_id = $1 AND identifier_hash = $2 AND reason = 'erased')",
    )
    .bind(workspace.as_uuid())
    .bind(identifier_hash.as_slice())
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(match (removed, erased) {
        (1.., _) => RemoveOutcome::Removed,
        (_, true) => RemoveOutcome::Erased,
        _ => RemoveOutcome::NotFound,
    })
}

pub async fn find_suppression<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    identifier_hash: &[u8; 32],
) -> Result<Option<Suppression>, StoreError> {
    let row: Option<SuppressionRow> = sqlx::query_as(
        "SELECT id, identifier_hash, kind, reason, source, note, created_at, created_by \
         FROM suppressions WHERE workspace_id = $1 AND identifier_hash = $2",
    )
    .bind(workspace.as_uuid())
    .bind(identifier_hash.as_slice())
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    row.map(SuppressionRow::into_suppression).transpose()
}

/// Whether the hash is suppressed in the workspace, for any reason.
pub async fn is_suppressed<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    identifier_hash: &[u8; 32],
) -> Result<bool, StoreError> {
    let found: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM suppressions \
                        WHERE workspace_id = $1 AND identifier_hash = $2)",
    )
    .bind(workspace.as_uuid())
    .bind(identifier_hash.as_slice())
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(found)
}

/// The position after a listed suppression: its time and id.
pub type SuppressionCursor = (OffsetDateTime, Uuid);

/// A page of suppressions, newest first by `(created_at, id)`, at most `limit`, after the
/// `before` cursor and optionally of one reason. The flag is true when more existed.
pub async fn list_suppressions<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    limit: u32,
    before: Option<SuppressionCursor>,
    reason: Option<SuppressionReason>,
) -> Result<(Vec<Suppression>, bool), StoreError> {
    let mut query = QueryBuilder::new(
        "SELECT id, identifier_hash, kind, reason, source, note, created_at, created_by \
         FROM suppressions WHERE workspace_id = ",
    );
    query.push_bind(workspace.as_uuid());
    if let Some(reason) = reason {
        query.push(" AND reason = ").push_bind(reason.as_str());
    }
    if let Some((created_at, id)) = before {
        query
            .push(" AND (created_at, id) < (")
            .push_bind(created_at)
            .push(", ")
            .push_bind(id)
            .push(")");
    }
    query
        .push(" ORDER BY created_at DESC, id DESC LIMIT ")
        .push_bind(i64::from(limit) + 1);
    let rows: Vec<SuppressionRow> = query
        .build_query_as()
        .fetch_all(executor)
        .await
        .map_err(StoreError::query)?;
    let more = rows.len() > limit as usize;
    let suppressions = rows
        .into_iter()
        .take(limit as usize)
        .map(SuppressionRow::into_suppression)
        .collect::<Result<_, _>>()?;
    Ok((suppressions, more))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::consent::{ChangeSource, Channel, ConsentStatus};
    use zirv_flow_core::people::{Identifier, identifier_hash};
    use zirv_flow_core::{
        Cause, HmacPurpose, HmacRoot, MasterKey, Slug, TraceContext, WorkspaceName,
    };

    use super::*;
    use crate::consent::{ConsentChange, set_consent};
    use crate::people::{ProfileOutcome, ProfileRecord, apply_profile_batch};
    use crate::{begin, commit, create_workspace};

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    fn hash(value: &str) -> [u8; 32] {
        let master = MasterKey::parse("BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=").unwrap();
        let key = HmacRoot::bootstrap_from(&master).hmac_key(HmacPurpose::Identifier);
        identifier_hash(&key, &Identifier::parse("email", value).unwrap())
    }

    fn manual(identifier_hash: [u8; 32]) -> NewSuppression<'static> {
        NewSuppression {
            identifier_hash,
            kind: "email",
            reason: SuppressionReason::Manual,
            source: "api",
            note: None,
            created_by: None,
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_suppression_is_added_found_and_listed_by_its_hash(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let ada = hash("ada@example.com");

        let first = add_suppression(&pool, ws, &manual(ada)).await.unwrap();

        assert_eq!(first, AddOutcome::Added);
        assert!(is_suppressed(&pool, ws, &ada).await.unwrap());
        assert!(
            !is_suppressed(&pool, ws, &hash("bob@example.com"))
                .await
                .unwrap()
        );
        let found = find_suppression(&pool, ws, &ada).await.unwrap().unwrap();
        assert_eq!(found.reason, SuppressionReason::Manual);
        assert_eq!(found.kind, "email");
        let (page, more) = list_suppressions(&pool, ws, 10, None, None).await.unwrap();
        assert!(!more);
        assert_eq!(page, [found]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn re_adding_keeps_the_first_reason_but_erased_overwrites_and_stays(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let ada = hash("ada@example.com");
        add_suppression(&pool, ws, &manual(ada)).await.unwrap();
        let mut bounce = manual(ada);
        bounce.reason = SuppressionReason::HardBounce;
        let mut erased = manual(ada);
        erased.reason = SuppressionReason::Erased;
        erased.source = "erasure";

        let again = add_suppression(&pool, ws, &bounce).await.unwrap();
        let reason_after_bounce = find_suppression(&pool, ws, &ada)
            .await
            .unwrap()
            .unwrap()
            .reason;
        let upgraded = add_suppression(&pool, ws, &erased).await.unwrap();
        let after_erase = find_suppression(&pool, ws, &ada).await.unwrap().unwrap();
        let downgraded = add_suppression(&pool, ws, &bounce).await.unwrap();
        let erased_again = add_suppression(&pool, ws, &erased).await.unwrap();

        assert_eq!(again, AddOutcome::Existing);
        assert_eq!(reason_after_bounce, SuppressionReason::Manual);
        assert_eq!(upgraded, AddOutcome::Upgraded);
        assert_eq!(after_erase.reason, SuppressionReason::Erased);
        assert_eq!(after_erase.source, "erasure");
        assert_eq!(downgraded, AddOutcome::Existing);
        assert_eq!(erased_again, AddOutcome::Existing);
        assert_eq!(
            find_suppression(&pool, ws, &ada)
                .await
                .unwrap()
                .unwrap()
                .reason,
            SuppressionReason::Erased
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_erased_suppression_cannot_be_removed_but_others_can(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let ada = hash("ada@example.com");
        let bob = hash("bob@example.com");
        add_suppression(&pool, ws, &manual(ada)).await.unwrap();
        let mut erased = manual(bob);
        erased.reason = SuppressionReason::Erased;
        add_suppression(&pool, ws, &erased).await.unwrap();

        let lifted = remove_suppression(&pool, ws, &ada).await.unwrap();
        let refused = remove_suppression(&pool, ws, &bob).await.unwrap();
        let unknown = remove_suppression(&pool, ws, &hash("zed@example.com"))
            .await
            .unwrap();

        assert_eq!(lifted, RemoveOutcome::Removed);
        assert_eq!(refused, RemoveOutcome::Erased);
        assert_eq!(unknown, RemoveOutcome::NotFound);
        assert!(!is_suppressed(&pool, ws, &ada).await.unwrap());
        assert!(is_suppressed(&pool, ws, &bob).await.unwrap());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_suppression_is_private_to_its_workspace(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let ada = hash("ada@example.com");
        add_suppression(&pool, one, &manual(ada)).await.unwrap();

        assert!(!is_suppressed(&pool, two, &ada).await.unwrap());
        assert!(find_suppression(&pool, two, &ada).await.unwrap().is_none());
        assert!(
            list_suppressions(&pool, two, 10, None, None)
                .await
                .unwrap()
                .0
                .is_empty()
        );
        assert_eq!(count_suppressed(&pool, two, &[ada]).await.unwrap(), 0);
        assert_eq!(
            remove_suppression(&pool, two, &ada).await.unwrap(),
            RemoveOutcome::NotFound
        );
        assert!(is_suppressed(&pool, one, &ada).await.unwrap());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_suppressed_identifier_stays_suppressed_by_its_hash_after_the_person_is_gone(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let record = ProfileRecord {
            source: "crm".into(),
            stream: "contacts".into(),
            record_key: "1".into(),
            identifiers: vec![("email".into(), "ada@example.com".into())],
            traits: serde_json::Map::new(),
            observed_at: datetime!(2026-01-01 00:00 UTC),
            trace: TraceContext::root(Cause::api("t")),
        };
        let ProfileOutcome::Applied { person_id, .. } = apply_profile_batch(&pool, ws, &[record])
            .await
            .unwrap()
            .remove(0)
        else {
            panic!("not applied");
        };
        let mut tx = begin(&pool).await.unwrap();
        set_consent(
            &mut tx,
            ws,
            &ConsentChange {
                person: person_id,
                channel: Channel::Email,
                topic: None,
                status: ConsentStatus::Subscribed,
                source: ChangeSource::Api,
                evidence: json!({}),
                occurred_at: datetime!(2026-01-01 00:00 UTC),
                actor: None,
            },
            &TraceContext::root(Cause::api("t")),
        )
        .await
        .unwrap();
        commit(tx).await.unwrap();
        let ada = hash("ada@example.com");
        let mut erased = manual(ada);
        erased.reason = SuppressionReason::Erased;
        add_suppression(&pool, ws, &erased).await.unwrap();

        for table in ["consent_changes", "consent", "identities", "source_records"] {
            sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM {table}")))
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::query("DELETE FROM person_changes")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM people")
            .execute(&pool)
            .await
            .unwrap();

        assert!(is_suppressed(&pool, ws, &ada).await.unwrap());
        assert_eq!(
            find_suppression(&pool, ws, &ada)
                .await
                .unwrap()
                .unwrap()
                .reason,
            SuppressionReason::Erased
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_listing_pages_newest_first_and_filters_by_reason(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        for (index, reason) in [
            SuppressionReason::Manual,
            SuppressionReason::HardBounce,
            SuppressionReason::Manual,
        ]
        .into_iter()
        .enumerate()
        {
            let mut new = manual(hash(&format!("p{index}@example.com")));
            new.reason = reason;
            add_suppression(&pool, ws, &new).await.unwrap();
            sqlx::query("UPDATE suppressions SET created_at = created_at + $1 * interval '1 second' WHERE identifier_hash = $2")
                .bind(i32::try_from(index).unwrap())
                .bind(new.identifier_hash.as_slice())
                .execute(&pool)
                .await
                .unwrap();
        }

        let (first, more) = list_suppressions(&pool, ws, 2, None, None).await.unwrap();
        let last = first.last().unwrap();
        let (second, more_after) =
            list_suppressions(&pool, ws, 2, Some((last.created_at, last.id)), None)
                .await
                .unwrap();
        let (bounces, _) =
            list_suppressions(&pool, ws, 10, None, Some(SuppressionReason::HardBounce))
                .await
                .unwrap();

        assert!(more && !more_after);
        assert_eq!(first.len(), 2);
        assert_eq!(second.len(), 1);
        assert!(first[0].created_at > first[1].created_at);
        assert!(first[1].created_at > second[0].created_at);
        assert_eq!(bounces.len(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_adds_only_the_new_hashes_and_counts_the_old_ones(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let (ada, bob, cy) = (
            hash("ada@example.com"),
            hash("bob@example.com"),
            hash("cy@example.com"),
        );
        add_suppression(&pool, ws, &manual(ada)).await.unwrap();

        let existing = count_suppressed(&pool, ws, &[ada, bob, cy]).await.unwrap();
        let added = add_suppressions(
            &pool,
            ws,
            &[
                BatchEntry {
                    identifier_hash: ada,
                    kind: "email",
                    note: None,
                },
                BatchEntry {
                    identifier_hash: bob,
                    kind: "email",
                    note: Some("from the old list"),
                },
                BatchEntry {
                    identifier_hash: cy,
                    kind: "phone",
                    note: None,
                },
            ],
            SuppressionReason::Manual,
            "import",
            None,
        )
        .await
        .unwrap();

        assert_eq!(existing, 1);
        assert_eq!(added, 2);
        assert_eq!(
            find_suppression(&pool, ws, &ada)
                .await
                .unwrap()
                .unwrap()
                .source,
            "api"
        );
        assert_eq!(
            find_suppression(&pool, ws, &bob)
                .await
                .unwrap()
                .unwrap()
                .note
                .as_deref(),
            Some("from the old list")
        );
        assert_eq!(
            find_suppression(&pool, ws, &cy)
                .await
                .unwrap()
                .unwrap()
                .kind,
            "phone"
        );
        assert_eq!(
            add_suppressions(&pool, ws, &[], SuppressionReason::Manual, "import", None)
                .await
                .unwrap(),
            0
        );
    }
}
