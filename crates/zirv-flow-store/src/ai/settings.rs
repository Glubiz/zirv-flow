//! A workspace's assistant settings. A workspace without a row has the assistant off.

use serde_json::Value;
use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{SecretId, UserId, WorkspaceId};

use crate::StoreError;

const COLUMNS: &str = "workspace_id, enabled, provider_id, model, monthly_budget_micros, \
     pii_opt_in, transcript_days, provider_key_secret_id, last_probe, last_probe_at, \
     updated_at, updated_by";

/// The outcome of the last capability probe, kept for the settings page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProbeRecord {
    /// `{"ok": bool, "missing": [{"capability", "reason"}]}`.
    pub result: Value,
    pub at: OffsetDateTime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AiSettings {
    pub workspace_id: WorkspaceId,
    pub enabled: bool,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub monthly_budget_micros: i64,
    pub pii_opt_in: bool,
    /// Days a transcript is kept; `None` stores no transcripts.
    pub transcript_days: Option<i32>,
    /// The sealed provider key, in `encrypted_secrets`.
    pub provider_key_secret_id: Option<SecretId>,
    pub last_probe: Option<ProbeRecord>,
    pub updated_at: Option<OffsetDateTime>,
    pub updated_by: Option<UserId>,
}

impl AiSettings {
    /// What a workspace without saved settings has: the assistant off, no budget, no personal
    /// data, no transcripts.
    pub fn defaults(workspace: WorkspaceId) -> Self {
        Self {
            workspace_id: workspace,
            enabled: false,
            provider_id: None,
            model: None,
            monthly_budget_micros: 0,
            pii_opt_in: false,
            transcript_days: None,
            provider_key_secret_id: None,
            last_probe: None,
            updated_at: None,
            updated_by: None,
        }
    }
}

/// The full state to store; a row is written whole.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AiSettingsUpdate {
    pub enabled: bool,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub monthly_budget_micros: i64,
    pub pii_opt_in: bool,
    pub transcript_days: Option<i32>,
    pub provider_key_secret_id: Option<SecretId>,
    /// `Some` replaces the stored probe; `None` keeps it.
    pub last_probe: Option<Value>,
    pub updated_by: Option<UserId>,
}

#[derive(FromRow)]
struct Row {
    workspace_id: Uuid,
    enabled: bool,
    provider_id: Option<String>,
    model: Option<String>,
    monthly_budget_micros: i64,
    pii_opt_in: bool,
    transcript_days: Option<i32>,
    provider_key_secret_id: Option<Uuid>,
    last_probe: Option<Value>,
    last_probe_at: Option<OffsetDateTime>,
    updated_at: OffsetDateTime,
    updated_by: Option<Uuid>,
}

impl From<Row> for AiSettings {
    fn from(row: Row) -> Self {
        Self {
            workspace_id: WorkspaceId::from_uuid(row.workspace_id),
            enabled: row.enabled,
            provider_id: row.provider_id,
            model: row.model,
            monthly_budget_micros: row.monthly_budget_micros,
            pii_opt_in: row.pii_opt_in,
            transcript_days: row.transcript_days,
            provider_key_secret_id: row.provider_key_secret_id.map(SecretId::from_uuid),
            last_probe: row
                .last_probe
                .zip(row.last_probe_at)
                .map(|(result, at)| ProbeRecord { result, at }),
            updated_at: Some(row.updated_at),
            updated_by: row.updated_by.map(UserId::from_uuid),
        }
    }
}

/// The saved settings of a workspace; `None` when it never saved any.
pub async fn find_ai_settings<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
) -> Result<Option<AiSettings>, StoreError> {
    let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM ai_settings WHERE workspace_id = $1"
    )))
    .bind(workspace.as_uuid())
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(row.map(Into::into))
}

/// As [`find_ai_settings`], locking the row until the transaction ends, so two admins saving at
/// once apply one after the other.
pub async fn lock_ai_settings(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<Option<AiSettings>, StoreError> {
    let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM ai_settings WHERE workspace_id = $1 FOR UPDATE"
    )))
    .bind(workspace.as_uuid())
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(row.map(Into::into))
}

/// Writes the settings of a workspace, creating the row on first save.
pub async fn put_ai_settings<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    update: &AiSettingsUpdate,
) -> Result<AiSettings, StoreError> {
    let row: Row = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO ai_settings (workspace_id, enabled, provider_id, model, \
             monthly_budget_micros, pii_opt_in, transcript_days, provider_key_secret_id, \
             last_probe, last_probe_at, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, \
                 CASE WHEN $9::jsonb IS NULL THEN NULL ELSE now() END, now(), $10) \
         ON CONFLICT (workspace_id) DO UPDATE SET \
             enabled = EXCLUDED.enabled, provider_id = EXCLUDED.provider_id, \
             model = EXCLUDED.model, monthly_budget_micros = EXCLUDED.monthly_budget_micros, \
             pii_opt_in = EXCLUDED.pii_opt_in, transcript_days = EXCLUDED.transcript_days, \
             provider_key_secret_id = EXCLUDED.provider_key_secret_id, \
             last_probe = COALESCE(EXCLUDED.last_probe, ai_settings.last_probe), \
             last_probe_at = CASE WHEN EXCLUDED.last_probe IS NULL \
                                  THEN ai_settings.last_probe_at ELSE now() END, \
             updated_at = now(), updated_by = EXCLUDED.updated_by \
         RETURNING {COLUMNS}"
    )))
    .bind(workspace.as_uuid())
    .bind(update.enabled)
    .bind(&update.provider_id)
    .bind(&update.model)
    .bind(update.monthly_budget_micros)
    .bind(update.pii_opt_in)
    .bind(update.transcript_days)
    .bind(update.provider_key_secret_id.map(|id| id.as_uuid()))
    .bind(&update.last_probe)
    .bind(update.updated_by.map(|id| id.as_uuid()))
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(row.into())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;
    use crate::create_workspace;

    async fn workspace(pool: &PgPool, name: &str) -> WorkspaceId {
        let name_parsed = WorkspaceName::parse(name).unwrap();
        let slug = Slug::parse(name).unwrap();
        create_workspace(pool, &name_parsed, &slug)
            .await
            .unwrap()
            .id
    }

    fn update() -> AiSettingsUpdate {
        AiSettingsUpdate {
            enabled: true,
            provider_id: Some("openai".into()),
            model: Some("gpt-x".into()),
            monthly_budget_micros: 5_000_000,
            pii_opt_in: true,
            transcript_days: Some(7),
            provider_key_secret_id: None,
            last_probe: Some(json!({"ok": true, "missing": []})),
            updated_by: None,
        }
    }

    #[test]
    fn the_defaults_have_the_assistant_off_with_nothing_allowed() {
        let settings = AiSettings::defaults(WorkspaceId::new());
        assert!(!settings.enabled);
        assert_eq!(settings.monthly_budget_micros, 0);
        assert!(!settings.pii_opt_in);
        assert_eq!(settings.transcript_days, None);
        assert!(settings.provider_key_secret_id.is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn settings_round_trip_and_a_second_put_updates_in_place(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        assert!(find_ai_settings(&pool, ws).await.unwrap().is_none());

        let saved = put_ai_settings(&pool, ws, &update()).await.unwrap();
        assert!(saved.enabled && saved.pii_opt_in);
        assert_eq!(saved.transcript_days, Some(7));
        assert_eq!(saved.model.as_deref(), Some("gpt-x"));
        assert_eq!(saved.last_probe.as_ref().unwrap().result["ok"], true);
        assert_eq!(find_ai_settings(&pool, ws).await.unwrap().unwrap(), saved);

        let changed = AiSettingsUpdate {
            enabled: false,
            transcript_days: None,
            last_probe: None,
            ..update()
        };
        let again = put_ai_settings(&pool, ws, &changed).await.unwrap();
        assert!(!again.enabled);
        assert_eq!(again.transcript_days, None);
        // No new probe: the stored one is kept.
        assert_eq!(again.last_probe, saved.last_probe);
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM ai_settings")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn settings_are_isolated_per_workspace(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        put_ai_settings(&pool, one, &update()).await.unwrap();

        assert!(find_ai_settings(&pool, one).await.unwrap().is_some());
        assert!(find_ai_settings(&pool, two).await.unwrap().is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_database_refuses_a_negative_budget_and_a_transcript_window_outside_1_to_30(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        for bad in [
            AiSettingsUpdate {
                monthly_budget_micros: -1,
                ..update()
            },
            AiSettingsUpdate {
                transcript_days: Some(0),
                ..update()
            },
            AiSettingsUpdate {
                transcript_days: Some(31),
                ..update()
            },
        ] {
            assert!(put_ai_settings(&pool, ws, &bad).await.is_err());
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn locking_returns_the_row_inside_a_transaction(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let mut tx = pool.begin().await.unwrap();
        assert!(lock_ai_settings(&mut tx, ws).await.unwrap().is_none());
        put_ai_settings(&mut *tx, ws, &update()).await.unwrap();
        assert!(lock_ai_settings(&mut tx, ws).await.unwrap().is_some());
        tx.commit().await.unwrap();
    }
}
