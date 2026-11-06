//! Locking people rows in id order, by primary key.
//!
//! The lock is taken on `people_pkey` and the workspace is checked on the locked rows, so the
//! plan never depends on planner statistics: a filter on `workspace_id` beside `id = ANY(..)`
//! lets a table without statistics (after a crash or a restore) choose `people_workspace_idx`
//! and walk the workspace's people to find the few that were asked for.
//!
//! A batch of records takes what its records will lock before the first one ([`lock_batch`]).

use std::collections::BTreeSet;

use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use super::resolve::MAX_MERGE_HOPS;
use crate::StoreError;

/// How many advisory locks a workspace's unowned identities are spread over. A batch takes at
/// most this many, whatever its size: advisory locks share the lock table with every other lock.
const IDENTITY_BUCKETS: u8 = 64;

/// A ledger row by its key.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LedgerKey<'a> {
    pub(crate) source: &'a str,
    pub(crate) stream: &'a str,
    pub(crate) record_key: &'a str,
}

/// The bucket of an identity: the first byte of the SHA-256 of its kind and value, modulo
/// [`IDENTITY_BUCKETS`].
fn identity_bucket(kind: &str, value: &str) -> i32 {
    let mut hash = Sha256::new();
    hash.update(kind.as_bytes());
    hash.update([0]);
    hash.update(value.as_bytes());
    i32::from(hash.finalize()[0] % IDENTITY_BUCKETS)
}

/// Takes, before a batch applies its first record, the locks its records would otherwise take one
/// at a time in the batch's order, which two batches over the same people take in opposite
/// orders (#518). In this order, until the transaction ends:
///
/// 1. the advisory lock of the bucket of each identity in `identities` (normalized `(kind,
///    value)` pairs) that nobody owns yet, by bucket: a record inserts those, and another batch
///    inserting one of them waits here instead of on the identity's unique key;
/// 2. the existing ledger rows of `ledger`, in id order;
/// 3. every person who owns one of the identities or one of those ledger rows, and every person
///    one of them was merged into, in id order.
///
/// What a record locks afterwards is held already, or a person it creates.
pub(super) async fn lock_batch(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    identities: &[(String, String)],
    ledger: &[LedgerKey<'_>],
) -> Result<(), StoreError> {
    let kinds: Vec<&str> = identities.iter().map(|(kind, _)| kind.as_str()).collect();
    let values: Vec<&str> = identities.iter().map(|(_, value)| value.as_str()).collect();
    let unowned: Vec<(String, String)> = sqlx::query_as(
        "SELECT t.kind, t.value FROM unnest($2::text[], $3::text[]) AS t(kind, value) \
         WHERE NOT EXISTS (SELECT 1 FROM identities i \
                           WHERE i.workspace_id = $1 AND i.kind = t.kind AND i.value = t.value)",
    )
    .bind(workspace.as_uuid())
    .bind(&kinds)
    .bind(&values)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let buckets: Vec<i32> = unowned
        .iter()
        .map(|(kind, value)| identity_bucket(kind, value))
        .collect::<BTreeSet<i32>>()
        .into_iter()
        .collect();
    if !buckets.is_empty() {
        sqlx::query(
            "SELECT pg_advisory_xact_lock( \
                 hashtextextended('identities/' || $1::text || '/' || t.bucket::text, 0)) \
             FROM unnest($2::int4[]) WITH ORDINALITY AS t(bucket, n) ORDER BY t.n",
        )
        .bind(workspace.as_uuid())
        .bind(&buckets)
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    }
    let sources: Vec<&str> = ledger.iter().map(|key| key.source).collect();
    let streams: Vec<&str> = ledger.iter().map(|key| key.stream).collect();
    let keys: Vec<&str> = ledger.iter().map(|key| key.record_key).collect();
    let mut wanted: Vec<Uuid> = sqlx::query_scalar(
        "WITH ledger AS MATERIALIZED ( \
             SELECT s.person_id FROM source_records s \
             JOIN unnest($4::text[], $5::text[], $6::text[]) AS l(source, stream, record_key) \
               ON s.source = l.source AND s.stream = l.stream AND s.record_key = l.record_key \
             WHERE s.workspace_id = $1 ORDER BY s.id FOR UPDATE OF s) \
         SELECT person_id FROM ledger \
         UNION \
         SELECT i.person_id FROM unnest($2::text[], $3::text[]) AS t(kind, value) \
         JOIN identities i ON i.workspace_id = $1 AND i.kind = t.kind AND i.value = t.value",
    )
    .bind(workspace.as_uuid())
    .bind(&kinds)
    .bind(&values)
    .bind(&sources)
    .bind(&streams)
    .bind(&keys)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let mut locked: BTreeSet<Uuid> = BTreeSet::new();
    for _ in 0..MAX_MERGE_HOPS {
        wanted.retain(|person| !locked.contains(person));
        wanted.sort_unstable();
        wanted.dedup();
        if wanted.is_empty() {
            break;
        }
        let rows = lock_people_in_order(connection, workspace, &wanted, PersonLock::Update).await?;
        locked.extend(wanted.iter().copied());
        wanted = rows
            .into_iter()
            .filter_map(|(_, merged_into)| merged_into)
            .collect();
    }
    Ok(())
}

/// The row-level lock a caller takes on the people it names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PersonLock {
    /// Keeps the row from being deleted or its key changed; other updates may go on.
    KeyShare,
    /// Keeps the row from being updated or deleted.
    Share,
    /// Excludes every other writer.
    Update,
}

impl PersonLock {
    /// The statement for this lock: `$1` is the workspace, `$2` the person ids. The ids are
    /// locked in a materialised step that sees no other condition, then the workspace is
    /// checked; a person of another workspace is neither returned nor acted on.
    pub(crate) const fn sql(self) -> &'static str {
        match self {
            Self::KeyShare => {
                "WITH locked AS MATERIALIZED ( \
                     SELECT id, workspace_id, merged_into FROM people \
                     WHERE id = ANY($2) ORDER BY id FOR KEY SHARE) \
                 SELECT id, merged_into FROM locked WHERE workspace_id = $1 ORDER BY id"
            }
            Self::Share => {
                "WITH locked AS MATERIALIZED ( \
                     SELECT id, workspace_id, merged_into FROM people \
                     WHERE id = ANY($2) ORDER BY id FOR SHARE) \
                 SELECT id, merged_into FROM locked WHERE workspace_id = $1 ORDER BY id"
            }
            Self::Update => {
                "WITH locked AS MATERIALIZED ( \
                     SELECT id, workspace_id, merged_into FROM people \
                     WHERE id = ANY($2) ORDER BY id FOR UPDATE) \
                 SELECT id, merged_into FROM locked WHERE workspace_id = $1 ORDER BY id"
            }
        }
    }
}

/// [`PersonLock::Update`] that also returns the `timezone` trait as written of each locked row.
const UPDATE_WITH_ZONE: &str = "WITH locked AS MATERIALIZED ( \
         SELECT id, workspace_id, merged_into, traits ->> 'timezone' AS zone FROM people \
         WHERE id = ANY($2) ORDER BY id FOR UPDATE) \
     SELECT id, merged_into, zone FROM locked WHERE workspace_id = $1 ORDER BY id";

/// Locks the people of `workspace` among `ids` in id order, and returns each with the person it
/// was merged into. A missing person, and one of another workspace, is absent.
pub(crate) async fn lock_people_in_order(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    ids: &[Uuid],
    lock: PersonLock,
) -> Result<Vec<(Uuid, Option<Uuid>)>, StoreError> {
    sqlx::query_as(lock.sql())
        .bind(workspace.as_uuid())
        .bind(ids)
        .fetch_all(connection)
        .await
        .map_err(StoreError::query)
}

/// [`lock_people_in_order`] with [`PersonLock::Update`], and each person's `timezone` trait as
/// written: read from the locked row, it stays what it is until the caller commits.
pub(crate) async fn lock_people_with_zones(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    ids: &[Uuid],
) -> Result<Vec<(Uuid, Option<Uuid>, Option<String>)>, StoreError> {
    sqlx::query_as(UPDATE_WITH_ZONE)
        .bind(workspace.as_uuid())
        .bind(ids)
        .fetch_all(connection)
        .await
        .map_err(StoreError::query)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use sqlx::PgPool;
    use zirv_flow_core::{Slug, WorkspaceName};

    use crate::create_workspace;

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    async fn insert_people(pool: &PgPool, workspace: WorkspaceId, ids: &[Uuid]) {
        sqlx::query("INSERT INTO people (id, workspace_id) SELECT unnest($2::uuid[]), $1")
            .bind(workspace.as_uuid())
            .bind(ids)
            .execute(pool)
            .await
            .unwrap();
    }

    fn node_types(plan: &Value, out: &mut Vec<(String, Option<String>)>) {
        if let Some(object) = plan.as_object() {
            if let Some(kind) = object.get("Node Type").and_then(Value::as_str) {
                let index = object
                    .get("Index Name")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                out.push((kind.to_owned(), index));
            }
            object.values().for_each(|child| node_types(child, out));
        } else if let Some(array) = plan.as_array() {
            array.iter().for_each(|child| node_types(child, out));
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_lock_plan_uses_the_primary_key_on_a_table_without_statistics(pool: PgPool) {
        let ws = workspace(&pool, "plan").await;
        // Enough rows that a sequential scan or the workspace index would be tempting, and
        // never analysed: a fresh database has no statistics.
        let ids: Vec<Uuid> = (0..5000).map(|_| Uuid::now_v7()).collect();
        insert_people(&pool, ws, &ids).await;
        let analysed: Option<time::OffsetDateTime> = sqlx::query_scalar(
            "SELECT coalesce(last_analyze, last_autoanalyze) FROM pg_stat_user_tables \
             WHERE relname = 'people'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(analysed.is_none(), "the table must have no statistics");
        let statements = [PersonLock::KeyShare, PersonLock::Share, PersonLock::Update]
            .map(|lock| (format!("{lock:?}"), lock.sql()))
            .into_iter()
            .chain([("UpdateWithZone".to_owned(), UPDATE_WITH_ZONE)]);
        for (lock, sql) in statements {
            let sql = format!("EXPLAIN (FORMAT JSON) {sql}");
            let plan: Value = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
                .bind(ws.as_uuid())
                .bind(&ids[..2])
                .fetch_one(&pool)
                .await
                .unwrap();
            let mut nodes = Vec::new();
            node_types(&plan, &mut nodes);
            let indexes: Vec<&str> = nodes
                .iter()
                .filter_map(|(_, index)| index.as_deref())
                .collect();
            assert_eq!(indexes, ["people_pkey"], "{lock}: {plan}");
            assert!(
                nodes.iter().all(|(kind, _)| !kind.contains("Seq Scan")),
                "{lock}: {plan}"
            );
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_zone_lock_returns_each_persons_timezone_trait_as_written(pool: PgPool) {
        let ws = workspace(&pool, "zones").await;
        let other = workspace(&pool, "elsewhere").await;
        let mut ids: Vec<Uuid> = (0..2).map(|_| Uuid::now_v7()).collect();
        ids.sort_unstable();
        let foreign = Uuid::now_v7();
        insert_people(&pool, ws, &ids).await;
        insert_people(&pool, other, &[foreign]).await;
        sqlx::query(
            "UPDATE people SET traits = '{\"timezone\": \"Not/AZone\"}', merged_into = $2 \
             WHERE id = $1",
        )
        .bind(ids[0])
        .bind(ids[1])
        .execute(&pool)
        .await
        .unwrap();
        let mut connection = pool.acquire().await.unwrap();

        let locked = lock_people_with_zones(&mut connection, ws, &[ids[1], foreign, ids[0]])
            .await
            .unwrap();

        assert_eq!(
            locked,
            [
                (ids[0], Some(ids[1]), Some("Not/AZone".to_owned())),
                (ids[1], None, None)
            ]
        );
    }

    #[test]
    fn an_identity_always_falls_in_the_same_one_of_the_buckets() {
        let bucket = identity_bucket("email", "a@example.com");

        assert!((0..i32::from(IDENTITY_BUCKETS)).contains(&bucket));
        assert_eq!(bucket, identity_bucket("email", "a@example.com"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_holds_the_owners_its_ledger_rows_and_the_buckets_of_its_new_identities(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "mine").await;
        let mut ids: Vec<Uuid> = (0..5).map(|_| Uuid::now_v7()).collect();
        ids.sort_unstable();
        let [owner, recorded, merged, survivor, unrelated] =
            <[Uuid; 5]>::try_from(ids.clone()).unwrap();
        insert_people(&pool, ws, &ids).await;
        sqlx::query("UPDATE people SET merged_into = $2 WHERE id = $1")
            .bind(merged)
            .bind(survivor)
            .execute(&pool)
            .await
            .unwrap();
        for (person, value) in [(owner, "known@example.com"), (merged, "stale@example.com")] {
            sqlx::query(
                "INSERT INTO identities (id, workspace_id, person_id, kind, value) \
                 VALUES ($1, $2, $3, 'email', $4)",
            )
            .bind(Uuid::now_v7())
            .bind(ws.as_uuid())
            .bind(person)
            .bind(value)
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO source_records \
                (id, workspace_id, source, stream, record_key, person_id, content_hash) \
             VALUES ($1, $2, 'crm', 'contacts', 'k1', $3, '\\x00')",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .bind(recorded)
        .execute(&pool)
        .await
        .unwrap();
        let known = identity_bucket("email", "known@example.com");
        let fresh = (0..)
            .map(|n| format!("new{n}@example.com"))
            .find(|value| identity_bucket("email", value) != known)
            .unwrap();
        let identities = [
            ("email".to_owned(), "known@example.com".to_owned()),
            ("email".to_owned(), "stale@example.com".to_owned()),
            ("email".to_owned(), fresh.clone()),
        ];
        let ledger = [
            LedgerKey {
                source: "crm",
                stream: "contacts",
                record_key: "k1",
            },
            LedgerKey {
                source: "crm",
                stream: "contacts",
                record_key: "never-seen",
            },
        ];
        let mut batch = pool.begin().await.unwrap();

        lock_batch(&mut batch, ws, &identities, &ledger)
            .await
            .unwrap();

        let mut other = pool.begin().await.unwrap();
        let free: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM people WHERE id = ANY($1) ORDER BY id FOR UPDATE SKIP LOCKED",
        )
        .bind(&ids)
        .fetch_all(&mut *other)
        .await
        .unwrap();
        assert_eq!(free, [unrelated]);
        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM (SELECT id FROM source_records FOR UPDATE SKIP LOCKED) free",
        )
        .fetch_one(&mut *other)
        .await
        .unwrap();
        assert_eq!(rows, 0, "the ledger row is held");
        let acquired = |bucket: i32| {
            sqlx::query_scalar::<_, bool>(
                "SELECT pg_try_advisory_xact_lock( \
                     hashtextextended('identities/' || $1::text || '/' || $2::text, 0))",
            )
            .bind(ws.as_uuid())
            .bind(bucket)
        };
        let new_bucket = acquired(identity_bucket("email", &fresh))
            .fetch_one(&mut *other)
            .await
            .unwrap();
        let owned_bucket = acquired(known).fetch_one(&mut *other).await.unwrap();
        assert!(!new_bucket, "the bucket of the new identity is held");
        assert!(owned_bucket, "an owned identity takes no bucket");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_lock_returns_the_workspaces_people_in_id_order_with_their_merge(pool: PgPool) {
        let ws = workspace(&pool, "mine").await;
        let other = workspace(&pool, "theirs").await;
        let mut ids: Vec<Uuid> = (0..3).map(|_| Uuid::now_v7()).collect();
        ids.sort_unstable();
        let foreign = Uuid::now_v7();
        insert_people(&pool, ws, &ids).await;
        insert_people(&pool, other, &[foreign]).await;
        sqlx::query("UPDATE people SET merged_into = $2 WHERE id = $1")
            .bind(ids[0])
            .bind(ids[2])
            .execute(&pool)
            .await
            .unwrap();
        let asked = [ids[2], foreign, ids[0], Uuid::now_v7(), ids[1]];
        for lock in [PersonLock::KeyShare, PersonLock::Share, PersonLock::Update] {
            let mut connection = pool.acquire().await.unwrap();

            let locked = lock_people_in_order(&mut connection, ws, &asked, lock)
                .await
                .unwrap();

            assert_eq!(
                locked,
                [(ids[0], Some(ids[2])), (ids[1], None), (ids[2], None)],
                "{lock:?}"
            );
        }
    }
}
