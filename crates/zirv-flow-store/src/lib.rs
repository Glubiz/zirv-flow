//! Persistence for zirv-flow.

mod accounts;
mod api_keys;
mod audit;
mod identities;
mod migrations;
mod pool;
mod sessions;
mod workspaces;

use std::error::Error;
use std::fmt;

pub use accounts::{
    User, count_owners, create_user, find_user, find_user_by_email, is_locked, list_users,
    record_failed_sign_in, record_successful_sign_in, set_password,
};
pub use api_keys::{
    ApiKey, StoredApiKey, create_api_key, find_api_key_for_auth, list_api_keys, revoke_api_key,
    touch_api_key,
};
pub use audit::{Actor, ActorKind, AuditEntry, NewAuditEntry, append_audit, list_audit};
pub use identities::{
    OidcLogin, create_oidc_login, delete_expired_oidc_logins, find_identity_user, link_identity,
    take_oidc_login,
};
pub use migrations::{SchemaState, migrate, schema_state};
pub use pool::{PgPool, Tx, begin, commit, connect};
pub use sessions::{
    ActiveSession, create_session, delete_expired_sessions, delete_session,
    delete_sessions_for_user, find_active_session, touch_session,
};
pub use sqlx::PgConnection;
pub use workspaces::{
    Member, Membership, Workspace, create_workspace, find_workspace, list_members,
    list_workspaces_for_user, membership_role, remove_membership, set_membership,
};

#[derive(Debug)]
pub struct StoreError {
    kind: StoreErrorKind,
    source: Option<Box<dyn Error + Send + Sync>>,
}

#[derive(Debug)]
enum StoreErrorKind {
    Connect,
    InspectSchema,
    ApplyMigrations,
    NewerSchema(i64),
    DirtyMigration(i64),
    ModifiedMigration(i64),
    Query,
    Conflict,
}

impl StoreError {
    fn connect(source: sqlx::Error) -> Self {
        Self::with_source(StoreErrorKind::Connect, source)
    }

    fn inspect_schema(source: sqlx::Error) -> Self {
        Self::with_source(StoreErrorKind::InspectSchema, source)
    }

    fn migration(source: sqlx::migrate::MigrateError) -> Self {
        use sqlx::migrate::MigrateError;

        match source {
            MigrateError::VersionMissing(version) => {
                Self::new(StoreErrorKind::NewerSchema(version))
            }
            MigrateError::Dirty(version) => Self::new(StoreErrorKind::DirtyMigration(version)),
            MigrateError::VersionMismatch(version) => {
                Self::new(StoreErrorKind::ModifiedMigration(version))
            }
            source => Self::with_source(StoreErrorKind::ApplyMigrations, source),
        }
    }

    fn apply_migrations(source: sqlx::Error) -> Self {
        Self::with_source(StoreErrorKind::ApplyMigrations, source)
    }

    /// A failed query. A unique-constraint violation becomes a conflict.
    pub(crate) fn query(source: sqlx::Error) -> Self {
        let conflict = source
            .as_database_error()
            .is_some_and(|error| error.is_unique_violation());
        let kind = if conflict {
            StoreErrorKind::Conflict
        } else {
            StoreErrorKind::Query
        };
        Self::with_source(kind, source)
    }

    /// Whether the write was refused because the value already exists.
    pub fn is_conflict(&self) -> bool {
        matches!(self.kind, StoreErrorKind::Conflict)
    }

    fn newer_schema(version: i64) -> Self {
        Self::new(StoreErrorKind::NewerSchema(version))
    }

    fn dirty_migration(version: i64) -> Self {
        Self::new(StoreErrorKind::DirtyMigration(version))
    }

    fn modified_migration(version: i64) -> Self {
        Self::new(StoreErrorKind::ModifiedMigration(version))
    }

    fn new(kind: StoreErrorKind) -> Self {
        Self { kind, source: None }
    }

    fn with_source(kind: StoreErrorKind, source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            kind,
            source: Some(Box::new(source)),
        }
    }
}

impl StoreError {
    fn write_source(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.source {
            Some(source) => write!(formatter, ": {source}"),
            None => Ok(()),
        }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            StoreErrorKind::Connect => {
                formatter.write_str("could not connect to PostgreSQL")?;
                self.write_source(formatter)
            }
            StoreErrorKind::InspectSchema => {
                formatter.write_str("could not inspect the database schema")?;
                self.write_source(formatter)
            }
            StoreErrorKind::ApplyMigrations => {
                formatter.write_str("could not apply database migrations")?;
                self.write_source(formatter)
            }
            StoreErrorKind::Query => {
                formatter.write_str("database query failed")?;
                self.write_source(formatter)
            }
            StoreErrorKind::Conflict => formatter.write_str("the value already exists"),
            StoreErrorKind::NewerSchema(version) => write!(
                formatter,
                "database schema version {version} is newer than this binary supports"
            ),
            StoreErrorKind::DirtyMigration(version) => {
                write!(formatter, "migration {version} is partially applied")
            }
            StoreErrorKind::ModifiedMigration(version) => write!(
                formatter,
                "migration {version} was changed after it was applied; restore the original migration file"
            ),
        }
    }
}

impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;

    /// Tables that hold instance-level data and so carry no `workspace_id`. Every other
    /// table is workspace-scoped and must have the column.
    const INSTANCE_LEVEL_TABLES: [&str; 7] = [
        "users",
        "user_identities",
        "oidc_logins",
        "workspaces",
        "sessions",
        "audit_log",
        "_sqlx_migrations",
    ];

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_table_without_a_workspace_id_is_on_the_instance_level_allowlist(pool: PgPool) {
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT t.table_name::text FROM information_schema.tables t              WHERE t.table_schema = current_schema() AND t.table_type = 'BASE TABLE'                AND NOT EXISTS (SELECT 1 FROM information_schema.columns c                                WHERE c.table_schema = t.table_schema                                  AND c.table_name = t.table_name                                  AND c.column_name = 'workspace_id')              ORDER BY 1",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        let unexpected: Vec<&String> = tables
            .iter()
            .filter(|table| !INSTANCE_LEVEL_TABLES.contains(&table.as_str()))
            .collect();
        assert!(
            unexpected.is_empty(),
            "tables without workspace_id that are not on the allowlist: {unexpected:?}"
        );
        assert!(tables.contains(&"users".to_owned()));
    }

    #[test]
    fn a_plain_query_error_is_not_a_conflict() {
        let error = StoreError::query(sqlx::Error::RowNotFound);

        assert!(!error.is_conflict());
        assert!(error.to_string().starts_with("database query failed"));
    }

    #[test]
    fn a_conflict_does_not_show_database_detail() {
        let error = StoreError::new(StoreErrorKind::Conflict);

        assert!(error.is_conflict());
        assert_eq!(error.to_string(), "the value already exists");
    }
}
