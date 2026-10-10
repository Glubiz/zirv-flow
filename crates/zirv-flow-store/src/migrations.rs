use std::collections::HashSet;

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

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn migrating_an_empty_database_brings_it_up_to_date(pool: PgPool) {
        let migrator = fixture("migrations-basic").await;

        assert_eq!(migrate_with(&pool, &migrator).await.unwrap(), vec![1, 2]);
        assert_eq!(
            schema_state_with(&pool, &migrator).await.unwrap(),
            SchemaState::UpToDate
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn migrating_twice_is_a_no_op(pool: PgPool) {
        let migrator = fixture("migrations-basic").await;

        migrate_with(&pool, &migrator).await.unwrap();

        assert!(migrate_with(&pool, &migrator).await.unwrap().is_empty());
    }

    #[sqlx::test]
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

    #[sqlx::test]
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

    #[sqlx::test]
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

    #[sqlx::test]
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

    #[sqlx::test]
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

    #[sqlx::test]
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
}
