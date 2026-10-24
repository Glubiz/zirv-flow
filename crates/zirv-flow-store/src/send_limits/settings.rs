//! The frequency caps and the quiet hours of a workspace, and the write that replaces both.

use sqlx::PgConnection;
use sqlx::prelude::FromRow;
use time::Time;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::StoreError;

/// What happens to a message that would exceed a cap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapMode {
    /// The message is skipped as `frequency_cap`.
    Skip,
    /// The message waits until a slot leaves the window.
    Defer,
}

impl CapMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::Defer => "defer",
        }
    }

    pub(crate) fn parse(text: &str) -> Result<Self, StoreError> {
        match text {
            "skip" => Ok(Self::Skip),
            "defer" => Ok(Self::Defer),
            _ => Err(StoreError::query(sqlx::Error::Decode(
                "unknown cap mode".into(),
            ))),
        }
    }
}

/// A cap: at most `limit` messages on `channel` to one person in any window of `window_hours`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrequencyCap {
    pub id: Uuid,
    pub channel: String,
    pub window_hours: i32,
    pub limit: i32,
    pub mode: CapMode,
}

/// A cap to store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewFrequencyCap {
    pub channel: String,
    pub window_hours: i32,
    pub limit: i32,
    pub mode: CapMode,
}

/// The quiet hours of a workspace: local times, and the weekdays (`mon` to `sun`) a quiet period
/// starts on. An end at or before the start is on the next day.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuietHours {
    pub start: Time,
    pub end: Time,
    pub days: Vec<String>,
}

/// Both settings of a workspace.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SendLimits {
    pub caps: Vec<FrequencyCap>,
    pub quiet_hours: Option<QuietHours>,
}

#[derive(FromRow)]
struct CapRow {
    id: Uuid,
    channel: String,
    window_hours: i32,
    message_limit: i32,
    mode: String,
}

#[derive(FromRow)]
struct QuietRow {
    start_time: Time,
    end_time: Time,
    days: Vec<String>,
}

impl TryFrom<CapRow> for FrequencyCap {
    type Error = StoreError;

    fn try_from(row: CapRow) -> Result<Self, StoreError> {
        Ok(Self {
            id: row.id,
            channel: row.channel,
            window_hours: row.window_hours,
            limit: row.message_limit,
            mode: CapMode::parse(&row.mode)?,
        })
    }
}

/// The workspace's caps (in the order they were written) and its quiet hours, if it set any.
pub async fn read_send_limits(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<SendLimits, StoreError> {
    let caps = sqlx::query_as::<_, CapRow>(
        "SELECT id, channel, window_hours, message_limit, mode FROM frequency_caps
         WHERE workspace_id = $1 ORDER BY id",
    )
    .bind(workspace.as_uuid())
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?
    .into_iter()
    .map(FrequencyCap::try_from)
    .collect::<Result<Vec<_>, _>>()?;
    let quiet_hours = sqlx::query_as::<_, QuietRow>(
        "SELECT start_time, end_time, days FROM quiet_hours WHERE workspace_id = $1",
    )
    .bind(workspace.as_uuid())
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?
    .map(|row| QuietHours {
        start: row.start_time,
        end: row.end_time,
        days: row.days,
    });
    Ok(SendLimits { caps, quiet_hours })
}

/// Replaces both settings. Run it in a transaction: the old settings are deleted and the new ones
/// written, so a reader sees one set or the other. Replaces of one workspace are serialised by a
/// lock on its row that the transaction holds to its end, so two concurrent replaces leave one
/// writer's settings.
pub async fn replace_send_limits(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    caps: &[NewFrequencyCap],
    quiet_hours: Option<&QuietHours>,
) -> Result<(), StoreError> {
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(workspace.as_uuid())
        .fetch_optional(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    sqlx::query("DELETE FROM frequency_caps WHERE workspace_id = $1")
        .bind(workspace.as_uuid())
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    sqlx::query("DELETE FROM quiet_hours WHERE workspace_id = $1")
        .bind(workspace.as_uuid())
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    for cap in caps {
        // A v7 id grows within a process, so reading by id returns the caps in the order given.
        sqlx::query(
            "INSERT INTO frequency_caps (id, workspace_id, channel, window_hours, message_limit, mode)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(Uuid::now_v7())
        .bind(workspace.as_uuid())
        .bind(&cap.channel)
        .bind(cap.window_hours)
        .bind(cap.limit)
        .bind(cap.mode.as_str())
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    }
    if let Some(quiet) = quiet_hours {
        sqlx::query(
            "INSERT INTO quiet_hours (workspace_id, start_time, end_time, days)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (workspace_id) DO UPDATE SET
                 start_time = EXCLUDED.start_time, end_time = EXCLUDED.end_time,
                 days = EXCLUDED.days",
        )
        .bind(workspace.as_uuid())
        .bind(quiet.start)
        .bind(quiet.end)
        .bind(&quiet.days)
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use time::macros::time;
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;

    async fn workspace(pool: &sqlx::PgPool) -> WorkspaceId {
        crate::migrate(pool).await.unwrap();
        let name = WorkspaceName::parse("ours").unwrap();
        let slug = Slug::parse("ours").unwrap();
        crate::create_workspace(pool, &name, &slug)
            .await
            .unwrap()
            .id
    }

    fn cap(limit: i32, mode: CapMode) -> NewFrequencyCap {
        NewFrequencyCap {
            channel: "email".to_owned(),
            window_hours: 24,
            limit,
            mode,
        }
    }

    #[test]
    fn a_cap_mode_round_trips_through_its_text() {
        for mode in [CapMode::Skip, CapMode::Defer] {
            assert_eq!(CapMode::parse(mode.as_str()).unwrap(), mode);
        }
        assert!(CapMode::parse("maybe").is_err());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn replacing_the_settings_swaps_both_and_keeps_the_order_of_the_caps(pool: sqlx::PgPool) {
        let ws = workspace(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        assert_eq!(
            read_send_limits(&mut conn, ws).await.unwrap(),
            SendLimits::default()
        );
        let quiet = QuietHours {
            start: time!(21:00),
            end: time!(08:00),
            days: vec!["mon".to_owned(), "fri".to_owned()],
        };
        replace_send_limits(
            &mut conn,
            ws,
            &[cap(3, CapMode::Skip), cap(1, CapMode::Defer)],
            Some(&quiet),
        )
        .await
        .unwrap();

        let read = read_send_limits(&mut conn, ws).await.unwrap();
        let limits: Vec<(i32, CapMode)> = read.caps.iter().map(|c| (c.limit, c.mode)).collect();
        assert_eq!(limits, vec![(3, CapMode::Skip), (1, CapMode::Defer)]);
        assert_eq!(read.quiet_hours, Some(quiet));

        replace_send_limits(&mut conn, ws, &[], None).await.unwrap();
        assert_eq!(
            read_send_limits(&mut conn, ws).await.unwrap(),
            SendLimits::default()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn concurrent_replaces_both_succeed_and_leave_one_writers_settings(pool: sqlx::PgPool) {
        let ws = workspace(&pool).await;
        let quiet = QuietHours {
            start: time!(21:00),
            end: time!(08:00),
            days: vec!["mon".to_owned()],
        };
        let write = |first: i32| {
            let pool = pool.clone();
            let quiet = quiet.clone();
            async move {
                let caps = [cap(first, CapMode::Skip), cap(first + 1, CapMode::Skip)];
                let mut tx = pool.begin().await.unwrap();
                replace_send_limits(&mut tx, ws, &caps, Some(&quiet))
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
            }
        };
        let (a, b) = (tokio::spawn(write(1)), tokio::spawn(write(7)));
        a.await.unwrap();
        b.await.unwrap();

        let mut conn = pool.acquire().await.unwrap();
        let read = read_send_limits(&mut conn, ws).await.unwrap();
        let limits: Vec<i32> = read.caps.iter().map(|c| c.limit).collect();
        assert!(limits == vec![1, 2] || limits == vec![7, 8], "{limits:?}");
        assert_eq!(read.quiet_hours, Some(quiet));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_table_refuses_equal_times_and_no_days(pool: sqlx::PgPool) {
        let ws = workspace(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let same = QuietHours {
            start: time!(08:00),
            end: time!(08:00),
            days: vec!["mon".to_owned()],
        };
        assert!(
            replace_send_limits(&mut conn, ws, &[], Some(&same))
                .await
                .is_err()
        );
        let none = QuietHours {
            start: time!(08:00),
            end: time!(09:00),
            days: Vec::new(),
        };
        assert!(
            replace_send_limits(&mut conn, ws, &[], Some(&none))
                .await
                .is_err()
        );
    }
}
