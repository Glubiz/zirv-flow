//! Persistence for zirv-flow.

mod migrations;
mod pool;

use std::error::Error;
use std::fmt;

pub use migrations::{SchemaState, migrate, schema_state};
pub use pool::{PgPool, connect};

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
