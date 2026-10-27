use hashbrown::HashSet;

use sqlx::PgConnection;
use sqlx::migrate::{Migrate, Migrator};

use crate::{PgPool, StoreError};

static MIGRATOR: Migrator = sqlx::migrate!();

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchemaState {
    UpToDate,
    Pending { count: usize },
    Newer { version: i64 },
    Dirty { version: i64 },
    Modified { version: i64 },
}

pub async fn migrate(pool: &PgPool) -> Result<Vec<i64>, StoreError> {
    migrate_with(pool, &MIGRATOR).await
}

pub async fn schema_state(pool: &PgPool) -> Result<SchemaState, StoreError> {
    schema_state_with(pool, &MIGRATOR).await
}

pub(crate) async fn migrate_with(
    pool: &PgPool,
    migrator: &Migrator,
) -> Result<Vec<i64>, StoreError> {
    let mut connection = pool.acquire().await.map_err(StoreError::apply_migrations)?;
    // The session may hold the advisory lock, so this connection must never return to the pool.
    connection.close_on_drop();
    connection.lock().await.map_err(StoreError::migration)?;

    let state = match load_schema_state(&mut connection, migrator).await {
        Ok(state) => state,
        Err(error) => {
            let _ = connection.unlock().await;
            return Err(error);
        }
    };
    let schema_error = match state.state {
        SchemaState::Newer { version } => Some(StoreError::newer_schema(version)),
        SchemaState::Dirty { version } => Some(StoreError::dirty_migration(version)),
        SchemaState::Modified { version } => Some(StoreError::modified_migration(version)),
        SchemaState::UpToDate | SchemaState::Pending { .. } => None,
    };
    if let Some(error) = schema_error {
        // The refusal is the error worth reporting; close_on_drop releases
        // the lock even if this unlock fails.
        let _ = connection.unlock().await;
        return Err(error);
    }

    let pending = migrator
        .iter()
        .filter(|migration| migration.migration_type.is_up_migration())
        .filter(|migration| !state.applied.contains(&migration.version))
        .map(|migration| migration.version)
        .collect();

    match migrator.run_direct(None, &mut *connection, false).await {
        Ok(()) => {
            connection.unlock().await.map_err(StoreError::migration)?;
            Ok(pending)
        }
        Err(error) => {
            // `run_direct` re-locks the reentrant lock and returns early on error, so unlock twice.
            let _ = connection.unlock().await;
            let _ = connection.unlock().await;
            Err(StoreError::migration(error))
        }
    }
}

pub(crate) async fn schema_state_with(
    pool: &PgPool,
    migrator: &Migrator,
) -> Result<SchemaState, StoreError> {
    let mut connection = pool.acquire().await.map_err(StoreError::inspect_schema)?;
    Ok(load_schema_state(&mut connection, migrator).await?.state)
}

struct LoadedSchemaState {
    state: SchemaState,
    applied: HashSet<i64>,
}

async fn load_schema_state(
    connection: &mut PgConnection,
    migrator: &Migrator,
) -> Result<LoadedSchemaState, StoreError> {
    let table_exists: bool =
        sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
            .fetch_one(&mut *connection)
            .await
            .map_err(StoreError::inspect_schema)?;

    let rows: Vec<(i64, bool, Vec<u8>)> = if table_exists {
        sqlx::query_as("SELECT version, success, checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&mut *connection)
            .await
            .map_err(StoreError::inspect_schema)?
    } else {
        Vec::new()
    };

    if let Some((version, _, _)) = rows.iter().find(|(_, success, _)| !success) {
        return Ok(LoadedSchemaState {
            state: SchemaState::Dirty { version: *version },
            applied: HashSet::new(),
        });
    }

    if let Some((version, _, _)) = rows
        .iter()
        .find(|(version, _, _)| !migrator.version_exists(*version))
    {
        return Ok(LoadedSchemaState {
            state: SchemaState::Newer { version: *version },
            applied: HashSet::new(),
        });
    }

    if let Some((version, _, _)) = rows.iter().find(|(version, _, checksum)| {
        migrator.iter().any(|migration| {
            migration.migration_type.is_up_migration()
                && migration.version == *version
                && migration.checksum.as_ref() != checksum.as_slice()
        })
    }) {
        return Ok(LoadedSchemaState {
            state: SchemaState::Modified { version: *version },
            applied: HashSet::new(),
        });
    }

    let applied = rows
        .into_iter()
        .map(|(version, _, _)| version)
        .collect::<HashSet<_>>();
    let pending = migrator
        .iter()
        .filter(|migration| migration.migration_type.is_up_migration())
        .filter(|migration| !applied.contains(&migration.version))
        .count();

    Ok(LoadedSchemaState {
        state: if pending == 0 {
            SchemaState::UpToDate
        } else {
            SchemaState::Pending { count: pending }
        },
        applied,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use super::*;

    async fn fixture(name: &str) -> Migrator {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name);
        Migrator::new(path).await.unwrap()
    }

    #[sqlx::test(migrations = false)]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn migrating_an_empty_database_brings_it_up_to_date(pool: PgPool) {
        let migrator = fixture("migrations-basic").await;

        assert_eq!(migrate_with(&pool, &migrator).await.unwrap(), vec![1, 2]);
        assert_eq!(
            schema_state_with(&pool, &migrator).await.unwrap(),
            SchemaState::UpToDate
        );
    }

    #[sqlx::test(migrations = false)]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn migrating_twice_is_a_no_op(pool: PgPool) {
        let migrator = fixture("migrations-basic").await;

        migrate_with(&pool, &migrator).await.unwrap();

        assert!(migrate_with(&pool, &migrator).await.unwrap().is_empty());
    }

    #[sqlx::test(migrations = false)]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_newer_schema_is_detected_and_migration_is_refused(pool: PgPool) {
        let migrator = fixture("migrations-basic").await;
        migrate_with(&pool, &migrator).await.unwrap();
        sqlx::query(
            "INSERT INTO _sqlx_migrations \
             (version, description, success, checksum, execution_time) \
             VALUES (999, 'future', true, '\\x00', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(
            schema_state_with(&pool, &migrator).await.unwrap(),
            SchemaState::Newer { version: 999 }
        );
        let error = migrate_with(&pool, &migrator).await.unwrap_err();
        assert!(
            error.to_string().contains("version 999 is newer"),
            "{error}"
        );
    }

    #[sqlx::test(migrations = false)]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn pending_migrations_are_counted(pool: PgPool) {
        let first = fixture("migrations-first").await;
        let all = fixture("migrations-basic").await;
        migrate_with(&pool, &first).await.unwrap();

        assert_eq!(
            schema_state_with(&pool, &all).await.unwrap(),
            SchemaState::Pending { count: 1 }
        );
    }

    #[sqlx::test(migrations = false)]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_dirty_migration_is_detected(pool: PgPool) {
        let migrator = fixture("migrations-first").await;
        migrate_with(&pool, &migrator).await.unwrap();
        sqlx::query(
            "INSERT INTO _sqlx_migrations \
             (version, description, success, checksum, execution_time) \
             VALUES (2, 'partial', false, '\\x00', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(
            schema_state_with(&pool, &migrator).await.unwrap(),
            SchemaState::Dirty { version: 2 }
        );
    }

    #[sqlx::test(migrations = false)]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn migrating_a_dirty_schema_is_refused(pool: PgPool) {
        let migrator = fixture("migrations-first").await;
        migrate_with(&pool, &migrator).await.unwrap();
        sqlx::query(
            "INSERT INTO _sqlx_migrations \
             (version, description, success, checksum, execution_time) \
             VALUES (2, 'partial', false, '\\x00', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let error = migrate_with(&pool, &migrator).await.unwrap_err();

        assert!(error.to_string().contains("partially applied"), "{error}");
    }

    #[sqlx::test(migrations = false)]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_modified_migration_is_detected_and_migration_is_refused(pool: PgPool) {
        let original = fixture("migrations-first").await;
        let modified = fixture("migrations-modified").await;
        migrate_with(&pool, &original).await.unwrap();

        assert_eq!(
            schema_state_with(&pool, &modified).await.unwrap(),
            SchemaState::Modified { version: 1 }
        );
        let error = migrate_with(&pool, &modified).await.unwrap_err();
        assert!(
            error.to_string().contains("migration 1 was changed"),
            "{error}"
        );
    }

    #[sqlx::test(migrations = false)]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn concurrent_migrators_apply_each_version_once(pool: PgPool) {
        let migrator = Arc::new(fixture("migrations-basic").await);
        let first = tokio::spawn({
            let pool = pool.clone();
            let migrator = Arc::clone(&migrator);
            async move { migrate_with(&pool, &migrator).await }
        });
        let second = tokio::spawn({
            let pool = pool.clone();
            let migrator = Arc::clone(&migrator);
            async move { migrate_with(&pool, &migrator).await }
        });

        let mut applied = vec![
            first.await.unwrap().unwrap(),
            second.await.unwrap().unwrap(),
        ];
        applied.sort();
        assert_eq!(applied, vec![vec![], vec![1, 2]]);

        let versions: Vec<i64> =
            sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(versions, vec![1, 2]);
    }

    #[sqlx::test(migrations = false)]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_trace_migration_applies_to_a_jobs_table_that_already_has_rows(pool: PgPool) {
        for earlier in [
            include_str!("../migrations/0001_accounts.sql"),
            include_str!("../migrations/0002_identities.sql"),
            include_str!("../migrations/0003_jobs.sql"),
        ] {
            sqlx::raw_sql(earlier).execute(&pool).await.unwrap();
        }
        sqlx::raw_sql(
            "INSERT INTO jobs (id, kind) VALUES (gen_random_uuid(), 'a'), (gen_random_uuid(), 'b')",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::raw_sql(include_str!("../migrations/0004_traces.sql"))
            .execute(&pool)
            .await
            .unwrap();

        let (distinct, kind, id): (i64, String, String) = sqlx::query_as(
            "SELECT count(DISTINCT trace_id), min(cause_kind), min(cause_id) FROM jobs",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((distinct, kind.as_str(), id.as_str()), (2, "manual", ""));
        let missing_default =
            sqlx::query("INSERT INTO jobs (id, kind) VALUES (gen_random_uuid(), 'c')")
                .execute(&pool)
                .await;
        assert!(
            missing_default.is_err(),
            "the backfill defaults must be dropped"
        );
    }

    #[sqlx::test(migrations = false)]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_view_pii_migration_gives_every_existing_membership_no_grant(pool: PgPool) {
        sqlx::raw_sql(include_str!("../migrations/0001_accounts.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(concat!(
            "INSERT INTO workspaces (id, name, slug) VALUES ('00000000-0000-0000-0000-0000000000a1', 'Acme', 'acme'); ",
            "INSERT INTO users (id, email, password_hash) VALUES ",
            "('00000000-0000-0000-0000-0000000000b1', 'admin@example.com', 'x'), ",
            "('00000000-0000-0000-0000-0000000000b2', 'member@example.com', 'x'); ",
            "INSERT INTO memberships (workspace_id, user_id, role) VALUES ",
            "('00000000-0000-0000-0000-0000000000a1', '00000000-0000-0000-0000-0000000000b1', 'admin'), ",
            "('00000000-0000-0000-0000-0000000000a1', '00000000-0000-0000-0000-0000000000b2', 'member')"
        ))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0073_membership_roles.sql"))
            .execute(&pool)
            .await
            .unwrap();

        sqlx::raw_sql(include_str!("../migrations/0078_membership_view_pii.sql"))
            .execute(&pool)
            .await
            .unwrap();

        let rows: Vec<(String, bool)> =
            sqlx::query_as("SELECT role, view_pii FROM memberships ORDER BY role")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            rows,
            vec![("admin".to_owned(), false), ("editor".to_owned(), false)]
        );
    }

    #[sqlx::test(migrations = false)]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_roles_migration_turns_members_into_editors_and_keeps_admins(pool: PgPool) {
        sqlx::raw_sql(include_str!("../migrations/0001_accounts.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(
            concat!(
                "INSERT INTO workspaces (id, name, slug) VALUES ('00000000-0000-0000-0000-0000000000a1', 'Acme', 'acme'); ",
                "INSERT INTO users (id, email, password_hash) VALUES ",
                "('00000000-0000-0000-0000-0000000000b1', 'admin@example.com', 'x'), ",
                "('00000000-0000-0000-0000-0000000000b2', 'member@example.com', 'x'); ",
                "INSERT INTO memberships (workspace_id, user_id, role) VALUES ",
                "('00000000-0000-0000-0000-0000000000a1', '00000000-0000-0000-0000-0000000000b1', 'admin'), ",
                "('00000000-0000-0000-0000-0000000000a1', '00000000-0000-0000-0000-0000000000b2', 'member'); ",
                "INSERT INTO api_keys (id, workspace_id, name, secret_hash, scopes) VALUES ",
                "('00000000-0000-0000-0000-0000000000c1', '00000000-0000-0000-0000-0000000000a1', 'admin', decode('01', 'hex'), ARRAY['read','admin']), ",
                "('00000000-0000-0000-0000-0000000000c2', '00000000-0000-0000-0000-0000000000a1', 'both', decode('02', 'hex'), ARRAY['publish','admin']), ",
                "('00000000-0000-0000-0000-0000000000c3', '00000000-0000-0000-0000-0000000000a1', 'write', decode('03', 'hex'), ARRAY['write']), ",
                "('00000000-0000-0000-0000-0000000000c4', '00000000-0000-0000-0000-0000000000a1', 'read', decode('04', 'hex'), ARRAY['read']), ",
                "('00000000-0000-0000-0000-0000000000c5', '00000000-0000-0000-0000-0000000000a1', 'nulls', decode('05', 'hex'), ARRAY['admin', NULL])"
            ),
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::raw_sql(include_str!("../migrations/0073_membership_roles.sql"))
            .execute(&pool)
            .await
            .unwrap();

        let roles: Vec<(String, String)> = sqlx::query_as(
            "SELECT u.email, m.role FROM memberships m JOIN users u ON u.id = m.user_id              ORDER BY u.email",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            roles,
            vec![
                ("admin@example.com".to_owned(), "admin".to_owned()),
                ("member@example.com".to_owned(), "editor".to_owned()),
            ]
        );
        let keys: Vec<(String, Vec<String>)> =
            sqlx::query_as("SELECT name, scopes FROM api_keys WHERE name <> 'nulls' ORDER BY name")
                .fetch_all(&pool)
                .await
                .unwrap();
        let scopes = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            keys,
            vec![
                ("admin".to_owned(), scopes(&["read", "admin", "publish"])),
                ("both".to_owned(), scopes(&["publish", "admin"])),
                ("read".to_owned(), scopes(&["read"])),
                ("write".to_owned(), scopes(&["write"])),
            ]
        );
        let null_element_key_gained: bool = sqlx::query_scalar(
            "SELECT array_position(scopes, 'publish') IS NOT NULL FROM api_keys WHERE name = 'nulls'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(null_element_key_gained, "an admin key with a null element");
        for role in ["publisher", "viewer"] {
            let allowed = sqlx::query("UPDATE memberships SET role = $1")
                .bind(role)
                .execute(&pool)
                .await;
            assert!(allowed.is_ok(), "{role}");
        }
        for role in ["member", "owner"] {
            let refused = sqlx::query("UPDATE memberships SET role = $1")
                .bind(role)
                .execute(&pool)
                .await;
            assert!(refused.is_err(), "{role}");
        }
    }

    const CAUSES: [&str; 6] = [
        "event",
        "schedule",
        "manual",
        "api",
        "definition_change",
        "sync_run",
    ];

    const TABLES: [&str; 4] = ["jobs", "decision_records", "person_changes", "enrollments"];

    /// One insert per table of `TABLES`, binding the cause kind as `$1`. The ids are made up:
    /// the tests turn foreign keys off for the session.
    const PLANTS: [&str; 4] = [
        "INSERT INTO jobs (id, kind, trace_id, cause_kind, cause_id) \
         VALUES (gen_random_uuid(), 'k', gen_random_uuid(), $1, 'c')",
        "INSERT INTO decision_records (id, workspace_id, trace_id, action, outcome, \
             cause_kind, cause_id) \
         VALUES (gen_random_uuid(), gen_random_uuid(), gen_random_uuid(), 'a', 'o', $1, 'c')",
        "INSERT INTO person_changes (workspace_id, person_id, cause_kind) \
         VALUES (gen_random_uuid(), gen_random_uuid(), $1)",
        "INSERT INTO enrollments (id, workspace_id, flow_id, flow_version, person_id, state, \
             node, occurrence_id, trace_id, cause_kind, cause_id) \
         VALUES (gen_random_uuid(), gen_random_uuid(), gen_random_uuid(), 1, \
             gen_random_uuid(), 'active', 'n', gen_random_uuid(), gen_random_uuid(), $1, 'c')",
    ];

    async fn without_foreign_keys(connection: &mut PgConnection) {
        sqlx::raw_sql("SET session_replication_role = replica")
            .execute(connection)
            .await
            .unwrap();
    }

    #[sqlx::test(migrations = false)]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_trait_change_migration_keeps_rows_of_every_other_cause(pool: PgPool) {
        let mut connection = pool.acquire().await.unwrap();
        MIGRATOR.run_to(81, &mut *connection).await.unwrap();
        without_foreign_keys(&mut connection).await;
        for cause in CAUSES {
            for plant in PLANTS {
                sqlx::query(plant)
                    .bind(cause)
                    .execute(&mut *connection)
                    .await
                    .unwrap();
            }
        }

        MIGRATOR.run(&mut *connection).await.unwrap();

        for table in TABLES {
            let kept: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT count(DISTINCT cause_kind) FROM {table}"
            )))
            .fetch_one(&mut *connection)
            .await
            .unwrap();
            assert_eq!(kept, 6, "{table}");
        }
    }

    #[sqlx::test(migrations = false)]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_trait_change_migration_is_refused_while_a_row_holds_the_cause(pool: PgPool) {
        let mut connection = pool.acquire().await.unwrap();
        MIGRATOR.run_to(81, &mut *connection).await.unwrap();
        without_foreign_keys(&mut connection).await;
        sqlx::query(PLANTS[0])
            .bind("trait_change")
            .execute(&mut *connection)
            .await
            .unwrap();

        let refused = MIGRATOR.run(&mut *connection).await;

        assert!(refused.is_err());
    }

    #[sqlx::test(migrations = false)]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn each_of_the_four_checks_refuses_trait_change_and_accepts_the_others(pool: PgPool) {
        let mut connection = pool.acquire().await.unwrap();
        MIGRATOR.run(&mut *connection).await.unwrap();
        without_foreign_keys(&mut connection).await;
        for (table, plant) in TABLES.iter().zip(PLANTS) {
            let refused = sqlx::query(plant)
                .bind("trait_change")
                .execute(&mut *connection)
                .await;
            assert!(refused.is_err(), "{table}");
            for cause in CAUSES {
                let accepted = sqlx::query(plant)
                    .bind(cause)
                    .execute(&mut *connection)
                    .await;
                assert!(accepted.is_ok(), "{table} {cause}");
            }
        }
    }
}
