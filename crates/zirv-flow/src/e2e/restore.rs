//! The restore drill (#398, exit criterion 5), in one process: a database copy stands in for the
//! backup, erasures made after it leave ledger files only, and `admin::replay_erasures` from the
//! ledger directory must make the restored database forget those people again.
//!
//! The copy is made with `CREATE DATABASE ... TEMPLATE`, which needs a database nobody is
//! connected to, so each phase ends by stopping the worker and closing the pool. Every database
//! the drill creates carries the prefix `zf_drill_` and is dropped by name when the drill ends.

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use axum::http::StatusCode;
    use serde_json::json;
    use sqlx::postgres::PgConnectOptions;
    use sqlx::{AssertSqlSafe, Connection, PgConnection, PgPool};
    use uuid::Uuid;
    use zirv_flow_api::admin::{self, Context};
    use zirv_flow_core::HmacRoot;
    use zirv_flow_store::{MasterKey, ReplayReport};

    use crate::e2e::harness::{PUBLIC_URL, World, call};

    const SCRATCH_PREFIX: &str = "zf_drill_";
    /// Two people that the drill merges into one, then erases together with the merge.
    const MERGED_A: &str = "merged-a@example.com";
    const MERGED_B: &str = "merged-b@example.com";
    const MERGED_B_EXTERNAL: &str = "crm-drill-b";
    /// Erased after the backup, never merged.
    const LATER: &str = "later@example.com";
    /// Erased before the backup: its ledger row is in the copy as well as in the directory.
    const EARLIER: &str = "earlier@example.com";
    /// Never erased.
    const SURVIVOR: &str = "survivor@example.com";

    /// A ledger directory that removes itself.
    struct LedgerDir(PathBuf);

    impl LedgerDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("zirv-flow-drill-ledger-{}", Uuid::now_v7()));
            fs::create_dir_all(&path).expect("a ledger directory");
            Self(path)
        }

        fn files(&self) -> usize {
            fs::read_dir(&self.0)
                .expect("the directory lists")
                .filter(|entry| {
                    entry
                        .as_ref()
                        .is_ok_and(|entry| entry.file_name().to_string_lossy().ends_with(".json"))
                })
                .count()
        }
    }

    impl Drop for LedgerDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// The scratch databases the drill created, dropped by name when it ends, also on a panic.
    struct Scratch {
        options: PgConnectOptions,
        names: Vec<String>,
    }

    impl Scratch {
        async fn admin(options: &PgConnectOptions) -> PgConnection {
            PgConnection::connect_with(&options.clone().database("postgres"))
                .await
                .expect("a connection to the maintenance database")
        }

        /// Copies `source` into a new scratch database, waiting for the last session on `source`
        /// to leave. Returns the new name.
        async fn copy_of(&mut self, source: &str) -> String {
            let name = format!("{SCRATCH_PREFIX}{}", Uuid::now_v7().simple());
            let mut connection = Self::admin(&self.options).await;
            let sql = format!("CREATE DATABASE \"{name}\" TEMPLATE \"{source}\"");
            for attempt in 0..100 {
                match sqlx::query(AssertSqlSafe(sql.clone()))
                    .execute(&mut connection)
                    .await
                {
                    Ok(_) => {
                        self.names.push(name.clone());
                        return name;
                    }
                    Err(sqlx::Error::Database(error))
                        if error.code().as_deref() == Some("55006") && attempt < 99 =>
                    {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Err(error) => panic!("the database copy failed: {error}"),
                }
            }
            unreachable!("the loop returns or panics")
        }

        fn pool_on(&self, name: &str) -> impl Future<Output = PgPool> + use<> {
            let options = self.options.clone().database(name);
            async move { PgPool::connect_with(options).await.expect("a pool") }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let options = self.options.clone();
            let names = std::mem::take(&mut self.names);
            let _ = std::thread::spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                runtime.block_on(async move {
                    let mut connection = Scratch::admin(&options).await;
                    for name in names {
                        assert!(name.starts_with(SCRATCH_PREFIX));
                        let _ = sqlx::query(AssertSqlSafe(format!(
                            "DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"
                        )))
                        .execute(&mut connection)
                        .await;
                    }
                });
            })
            .join();
        }
    }

    fn root() -> HmacRoot {
        HmacRoot::bootstrap_from(
            &MasterKey::parse("BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=").expect("key"),
        )
    }

    async fn count(pool: &PgPool, sql: &str, ids: &[Uuid]) -> i64 {
        sqlx::query_scalar(AssertSqlSafe(sql.to_owned()))
            .bind(ids)
            .fetch_one(pool)
            .await
            .expect("a count")
    }

    async fn person_of(pool: &PgPool, address: &str) -> Uuid {
        sqlx::query_scalar("SELECT person_id FROM identities WHERE kind = 'email' AND value = $1")
            .bind(address)
            .fetch_one(pool)
            .await
            .expect("the person of the address")
    }

    async fn identify(world: &World, identifiers: serde_json::Value) {
        let reply = call(
            &world.router,
            "POST",
            "/api/v1/ingest/identify",
            Some(json!({"identifiers": identifiers})),
            Some(&world.ingest),
        )
        .await;
        assert_eq!(reply.status, StatusCode::ACCEPTED, "{}", reply.text);
        world.settle().await;
    }

    async fn erase(world: &World, person: Uuid) {
        world
            .api_ok(
                "POST",
                "/privacy/erasures",
                Some(json!({"person_id": person})),
                StatusCode::ACCEPTED,
            )
            .await;
        world.settle().await;
    }

    /// The tables a replay must not grow, as counts.
    async fn totals(pool: &PgPool) -> Vec<i64> {
        let mut out = Vec::new();
        for table in [
            "people",
            "identities",
            "suppressions",
            "erasure_requests",
            "ingest_inbox",
        ] {
            let sql = format!("SELECT count(*) FROM {table} WHERE $1::uuid[] IS NOT NULL");
            out.push(count(pool, &sql, &[]).await);
        }
        out
    }

    /// What the drill holds once the database is restored and before the replay.
    struct Restored {
        pool: PgPool,
        dir: LedgerDir,
        /// The ids that must be gone after the replay: the merged group and the later person.
        erased: Vec<Uuid>,
        survivor: Uuid,
        _scratch: Scratch,
    }

    /// Populates a workspace, backs it up as a database copy, erases people after the backup,
    /// and restores by pointing a pool at the copy.
    async fn restored(live: PgPool) -> Restored {
        let options = (*live.connect_options()).clone();
        let live_name = options.get_database().expect("a database name").to_owned();
        let mut scratch = Scratch {
            options,
            names: Vec::new(),
        };
        let dir = LedgerDir::new();

        let world = World::start_with_ledger(live.clone(), PUBLIC_URL, &dir.0).await;
        identify(&world, json!({"email": MERGED_A})).await;
        identify(
            &world,
            json!({"email": MERGED_B, "external:crm": MERGED_B_EXTERNAL}),
        )
        .await;
        identify(
            &world,
            json!({"email": MERGED_A, "external:crm": MERGED_B_EXTERNAL}),
        )
        .await;
        identify(&world, json!({"email": LATER})).await;
        identify(&world, json!({"email": EARLIER})).await;
        identify(&world, json!({"email": SURVIVOR})).await;
        let survivor = person_of(&live, SURVIVOR).await;
        world
            .api_ok(
                "PUT",
                &format!("/people/{survivor}/consent"),
                Some(json!({"channel": "email", "status": "subscribed"})),
                StatusCode::OK,
            )
            .await;
        let earlier = person_of(&live, EARLIER).await;
        erase(&world, earlier).await;
        assert_eq!(dir.files(), 1, "the first erasure wrote its ledger file");

        // A pending inbox row of a person who is erased later: the worker is stopped first, so it
        // stays pending, as in a backup taken while the drain is behind.
        let router = world.router.clone();
        let ingest = world.ingest.clone();
        world.finish().await;
        let queued = call(
            &router,
            "POST",
            "/api/v1/ingest/identify",
            Some(json!({"identifiers": {"email": LATER}, "traits": {"plan": "free"}})),
            Some(&ingest),
        )
        .await;
        assert_eq!(queued.status, StatusCode::ACCEPTED, "{}", queued.text);
        live.close().await;

        // The backup.
        let backup = scratch.copy_of(&live_name).await;

        // The erasures after it: the live database learns of them, the backup does not.
        let live = scratch.pool_on(&live_name).await;
        let world = World::resume(live.clone(), PUBLIC_URL, &dir.0).await;
        let merged = person_of(&live, MERGED_A).await;
        let group: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM people WHERE id = $1 OR merged_into = $1 ORDER BY id",
        )
        .bind(merged)
        .fetch_all(&live)
        .await
        .expect("the group");
        assert_eq!(group.len(), 2, "the two people were merged into one group");
        let later = person_of(&live, LATER).await;
        erase(&world, merged).await;
        erase(&world, later).await;
        assert_eq!(dir.files(), 3, "each erasure wrote a ledger file");
        world.finish().await;
        live.close().await;

        // The restore: the live data is dropped by switching to the copy.
        let pool = scratch.pool_on(&backup).await;
        let mut erased = group;
        erased.push(later);
        Restored {
            pool,
            dir,
            erased,
            survivor,
            _scratch: scratch,
        }
    }

    async fn replay(restored: &Restored) -> ReplayReport {
        admin::replay_erasures(
            &restored.pool,
            &Context::system(),
            Some("acme"),
            &root(),
            Some(&restored.dir.0),
        )
        .await
        .expect("the replay")
    }

    async fn present(pool: &PgPool, ids: &[Uuid]) -> i64 {
        count(
            pool,
            "SELECT count(*) FROM people WHERE id = ANY($1)",
            ids,
        )
        .await
    }

    async fn erased_suppressions(pool: &PgPool) -> i64 {
        count(
            pool,
            "SELECT count(*) FROM suppressions WHERE reason = 'erased' AND $1::uuid[] IS NOT NULL",
            &[],
        )
        .await
    }

    async fn pending_inbox(pool: &PgPool) -> i64 {
        count(
            pool,
            "SELECT count(*) FROM ingest_inbox WHERE $1::uuid[] IS NOT NULL",
            &[],
        )
        .await
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_erased_person_stays_erased_after_the_restore_and_the_replay_applies_a_later_erasure(
        live: PgPool,
    ) {
        let drill = restored(live).await;
        let pool = &drill.pool;
        // The restored database holds what was erased after the backup, and knows only one erasure.
        assert_eq!(present(pool, &drill.erased).await, 3);
        assert_eq!(
            count(
                pool,
                "SELECT count(*) FROM erasure_requests WHERE $1::uuid[] IS NOT NULL",
                &[]
            )
            .await,
            1
        );
        assert_eq!(erased_suppressions(pool).await, 1);
        assert_eq!(pending_inbox(pool).await, 1);

        let report = replay(&drill).await;

        assert_eq!(report.merged, 2, "the two later ledger files are merged in");
        assert_eq!(report.requests, 3);
        assert_eq!(report.people, 2, "the merged group counts once, and the later person");
        assert_eq!(report.suppressions, 3);
        assert_eq!(report.inbox_rows, 1);
        assert_eq!(report.skipped_workspaces, 0);
        assert_eq!(present(pool, &drill.erased).await, 0);
        assert_eq!(
            count(
                pool,
                "SELECT count(*) FROM identities WHERE person_id = ANY($1)",
                &drill.erased
            )
            .await,
            0
        );
        assert_eq!(erased_suppressions(pool).await, 4);
        assert_eq!(pending_inbox(pool).await, 0);
        assert_eq!(
            count(
                pool,
                "SELECT count(*) FROM erasure_requests \
                 WHERE status = 'completed' AND $1::uuid[] IS NOT NULL",
                &[]
            )
            .await,
            3
        );
        let survivor = [drill.survivor];
        assert_eq!(present(pool, &survivor).await, 1);
        assert_eq!(
            count(
                pool,
                "SELECT count(*) FROM identities WHERE person_id = ANY($1)",
                &survivor
            )
            .await,
            1
        );
        assert_eq!(
            count(
                pool,
                "SELECT count(*) FROM consent WHERE status = 'subscribed' AND person_id = ANY($1)",
                &survivor
            )
            .await,
            1
        );
        assert_eq!(
            count(
                pool,
                "SELECT count(*) FROM audit_log WHERE action = 'privacy.erasures_replayed' \
                 AND $1::uuid[] IS NOT NULL",
                &[]
            )
            .await,
            1
        );
        drill.pool.close().await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_second_replay_changes_nothing_and_re_creates_nothing(live: PgPool) {
        let drill = restored(live).await;
        replay(&drill).await;
        let after_first = totals(&drill.pool).await;

        let second = replay(&drill).await;

        assert_eq!(totals(&drill.pool).await, after_first);
        assert_eq!(second.merged, 0, "every file is already in the ledger");
        assert_eq!(second.requests, 3);
        assert_eq!(second.people, 0);
        assert_eq!(second.suppressions, 0);
        assert_eq!(second.inbox_rows, 0);
        assert_eq!(present(&drill.pool, &drill.erased).await, 0);
        drill.pool.close().await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_ledger_file_of_a_bad_shape_is_named_and_the_others_apply_once_it_is_moved_aside(
        live: PgPool,
    ) {
        let drill = restored(live).await;
        let workspace: Uuid = sqlx::query_scalar("SELECT id FROM workspaces WHERE slug = 'acme'")
            .fetch_one(&drill.pool)
            .await
            .expect("the workspace");
        let bad_name = format!("{workspace}-{}.json", Uuid::now_v7());
        let bad: &Path = &drill.dir.0.join(&bad_name);
        fs::write(bad, b"{\"format\": \"zirv-flow-ledger/1\", \"id\": 7}").expect("a bad file");
        let before = totals(&drill.pool).await;

        let refused = admin::replay_erasures(
            &drill.pool,
            &Context::system(),
            Some("acme"),
            &root(),
            Some(&drill.dir.0),
        )
        .await
        .expect_err("a bad file stops the read");

        assert!(
            refused.to_string().contains(&bad_name),
            "the error names the file: {refused}"
        );
        assert_eq!(totals(&drill.pool).await, before, "nothing was applied");
        assert_eq!(present(&drill.pool, &drill.erased).await, 3);

        fs::remove_file(bad).expect("the bad file moves aside");
        let report = replay(&drill).await;

        assert_eq!(report.merged, 2);
        assert_eq!(present(&drill.pool, &drill.erased).await, 0);
        drill.pool.close().await;
    }
}
