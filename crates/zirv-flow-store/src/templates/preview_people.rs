//! Saved preview people (architecture 11.7 and 16.3): the people an admin keeps on a template to
//! preview it as. The table is person-scoped: an erasure deletes a person's rows and a merge moves
//! them to the survivor ([`move_to_survivor`]).

use std::fmt;

use sqlx::PgConnection;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{UserId, WorkspaceId};

use super::TemplateError;
use super::crud::lock_live_template;
use crate::StoreError;
use crate::people::{current_person, strongest_identifiers};

/// The most people a template keeps.
pub const MAX_PREVIEW_PEOPLE: i64 = 20;

/// A saved preview person with the identifier to show for them.
#[derive(Clone, Debug, PartialEq)]
pub struct PreviewPerson {
    pub person_id: Uuid,
    /// The strongest identifier as `(kind, value)`; none for a person with no identifier. Personal
    /// data: the caller masks it.
    pub display: Option<(String, String)>,
    pub created_at: OffsetDateTime,
}

#[derive(FromRow)]
struct Row {
    person_id: Uuid,
    created_at: OffsetDateTime,
}

/// Why a preview person was not saved.
#[derive(Debug)]
pub enum PreviewPersonError {
    /// No such template, or no such person, in the workspace.
    NotFound,
    /// The template is archived and no longer changes.
    Archived,
    /// The template already keeps [`MAX_PREVIEW_PEOPLE`] people.
    Limit,
    Store(StoreError),
}

impl From<StoreError> for PreviewPersonError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<TemplateError> for PreviewPersonError {
    fn from(error: TemplateError) -> Self {
        match error {
            TemplateError::NotFound => Self::NotFound,
            TemplateError::Archived => Self::Archived,
            TemplateError::Store(error) => Self::Store(error),
        }
    }
}

impl fmt::Display for PreviewPersonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("no such template or person"),
            Self::Archived => formatter.write_str("the template is archived"),
            Self::Limit => formatter.write_str("the template keeps the most preview people"),
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for PreviewPersonError {}

/// The template's saved people, oldest first. None when the template does not exist.
pub async fn list_preview_people(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    template: Uuid,
) -> Result<Option<Vec<PreviewPerson>>, StoreError> {
    let exists: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM templates WHERE workspace_id = $1 AND id = $2")
            .bind(workspace.as_uuid())
            .bind(template)
            .fetch_optional(&mut *connection)
            .await
            .map_err(StoreError::query)?;
    if exists.is_none() {
        return Ok(None);
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT person_id, created_at FROM template_preview_people \
         WHERE workspace_id = $1 AND template_id = $2 ORDER BY created_at, person_id",
    )
    .bind(workspace.as_uuid())
    .bind(template)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let ids: Vec<Uuid> = rows.iter().map(|row| row.person_id).collect();
    let mut display = strongest_identifiers(connection, workspace, &ids).await?;
    Ok(Some(
        rows.into_iter()
            .map(|row| PreviewPerson {
                display: display.remove(&row.person_id),
                person_id: row.person_id,
                created_at: row.created_at,
            })
            .collect(),
    ))
}

/// How many merged-away hops [`lock_current_person`] follows before it gives up.
const MERGE_HOPS: usize = 3;

/// Resolves `person` to the person they are now and locks that row `FOR KEY SHARE` until the
/// transaction ends. A merge locks its group `FOR UPDATE` before it moves saved rows, so it waits
/// for this transaction and then sees the row written under the lock; one that committed first is
/// found by the re-read after the lock, which follows `merged_into` to the survivor. The foreign
/// key alone would not do: it takes the same lock only at the insert, after the merge has looked.
async fn lock_current_person(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
) -> Result<Option<Uuid>, StoreError> {
    let mut target = person;
    for _ in 0..MERGE_HOPS {
        let Some(current) = current_person(&mut *connection, workspace, target).await? else {
            return Ok(None);
        };
        sqlx::query("SELECT id FROM people WHERE workspace_id = $1 AND id = $2 FOR KEY SHARE")
            .bind(workspace.as_uuid())
            .bind(current)
            .fetch_optional(&mut *connection)
            .await
            .map_err(StoreError::query)?;
        // A locking read can return the row as it was before a merge that committed during the
        // wait, so ask again with a plain read.
        let merged_into: Option<Option<Uuid>> = sqlx::query_scalar(
            "SELECT merged_into FROM people WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace.as_uuid())
        .bind(current)
        .fetch_optional(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        match merged_into {
            Some(None) => return Ok(Some(current)),
            Some(Some(survivor)) => target = survivor,
            None => return Ok(None),
        }
    }
    Err(StoreError::retryable_race())
}

/// Saves `person` (the survivor, when it was merged away) on the template, in the caller's
/// transaction. The second value is false when the person was already saved. The person is locked
/// against a concurrent merge, and then the template row, so two adds cannot both pass the limit.
pub async fn add_preview_person(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    template: Uuid,
    person: Uuid,
    added_by: Option<UserId>,
) -> Result<(PreviewPerson, bool), PreviewPersonError> {
    // The person first, then the template: a merge holds the person and then writes rows that
    // refer to the template, so the other order could deadlock with it.
    let person = lock_current_person(connection, workspace, person)
        .await?
        .ok_or(PreviewPersonError::NotFound)?;
    lock_live_template(connection, workspace, template).await?;
    let held: Option<Row> = sqlx::query_as(
        "SELECT person_id, created_at FROM template_preview_people \
         WHERE template_id = $1 AND person_id = $2",
    )
    .bind(template)
    .bind(person)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let (row, created) = match held {
        Some(row) => (row, false),
        None => {
            let kept: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM template_preview_people WHERE template_id = $1",
            )
            .bind(template)
            .fetch_one(&mut *connection)
            .await
            .map_err(StoreError::query)?;
            if kept >= MAX_PREVIEW_PEOPLE {
                return Err(PreviewPersonError::Limit);
            }
            let row: Row = sqlx::query_as(
                "INSERT INTO template_preview_people \
                    (template_id, workspace_id, person_id, added_by) VALUES ($1, $2, $3, $4) \
                 RETURNING person_id, created_at",
            )
            .bind(template)
            .bind(workspace.as_uuid())
            .bind(person)
            .bind(added_by.map(|user| user.as_uuid()))
            .fetch_one(&mut *connection)
            .await
            .map_err(StoreError::query)?;
            (row, true)
        }
    };
    let display = strongest_identifiers(connection, workspace, &[person])
        .await?
        .remove(&person);
    Ok((
        PreviewPerson {
            person_id: row.person_id,
            display,
            created_at: row.created_at,
        },
        created,
    ))
}

/// Removes a saved person from the template. False when the template did not keep them.
pub async fn remove_preview_person(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    template: Uuid,
    person: Uuid,
) -> Result<bool, StoreError> {
    let removed = sqlx::query(
        "DELETE FROM template_preview_people \
         WHERE workspace_id = $1 AND template_id = $2 AND person_id = $3",
    )
    .bind(workspace.as_uuid())
    .bind(template)
    .bind(person)
    .execute(connection)
    .await
    .map_err(StoreError::query)?
    .rows_affected();
    Ok(removed > 0)
}

/// Moves the losers' saved rows to the survivor, in the merge's transaction. A template that
/// already keeps the survivor just loses the loser's row. A survivor can pass the limit when it
/// takes over rows of several losers on one template; the merge does not refuse for that.
pub(crate) async fn move_to_survivor(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    losers: &[Uuid],
    survivor: Uuid,
) -> Result<(), StoreError> {
    sqlx::query(
        "WITH moved AS ( \
             DELETE FROM template_preview_people \
             WHERE workspace_id = $1 AND person_id = ANY($2) \
             RETURNING template_id, added_by, created_at) \
         INSERT INTO template_preview_people \
             (template_id, workspace_id, person_id, added_by, created_at) \
         SELECT DISTINCT ON (template_id) template_id, $1, $3, added_by, created_at \
         FROM moved ORDER BY template_id, created_at \
         ON CONFLICT (template_id, person_id) DO NOTHING",
    )
    .bind(workspace.as_uuid())
    .bind(losers)
    .bind(survivor)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::{Cause, TraceContext};

    use super::*;
    use crate::templates::tests_support::{create, workspace};
    use crate::{ProfileOutcome, ProfileRecord, apply_profile_batch};

    async fn person(pool: &PgPool, ws: WorkspaceId, key: &str) -> Uuid {
        let record = ProfileRecord {
            source: "crm".into(),
            stream: "s".into(),
            record_key: key.into(),
            identifiers: vec![("email".into(), format!("{key}@example.test"))],
            traits: json!({}).as_object().cloned().unwrap(),
            observed_at: datetime!(2026-01-01 00:00 UTC),
            trace: TraceContext::root(Cause::api("test")),
        };
        match apply_profile_batch(pool, ws, &[record]).await.unwrap()[0] {
            ProfileOutcome::Applied { person_id, .. }
            | ProfileOutcome::Unchanged { person_id, .. } => person_id,
            ProfileOutcome::Rejected { .. } => panic!("rejected"),
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn people_are_listed_oldest_first_with_their_identifier_and_removed_on_request(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let template = create(&pool, ws, "Welcome").await;
        let (first, second) = (person(&pool, ws, "a").await, person(&pool, ws, "b").await);
        let mut connection = pool.acquire().await.unwrap();
        for saved in [second, first] {
            let (_, created) = add_preview_person(&mut connection, ws, template.id, saved, None)
                .await
                .unwrap();
            assert!(created);
        }

        let listed = list_preview_people(&mut connection, ws, template.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            listed.iter().map(|p| p.person_id).collect::<Vec<_>>(),
            [second, first]
        );
        assert_eq!(
            listed[0].display,
            Some(("email".into(), "b@example.test".into()))
        );
        assert!(
            remove_preview_person(&mut connection, ws, template.id, second)
                .await
                .unwrap()
        );
        assert!(
            !remove_preview_person(&mut connection, ws, template.id, second)
                .await
                .unwrap()
        );
        assert!(
            list_preview_people(&mut connection, ws, Uuid::now_v7())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn adding_a_saved_person_again_keeps_one_row(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let template = create(&pool, ws, "Welcome").await;
        let saved = person(&pool, ws, "a").await;
        let mut connection = pool.acquire().await.unwrap();

        let (first, created) = add_preview_person(&mut connection, ws, template.id, saved, None)
            .await
            .unwrap();
        let (again, created_again) =
            add_preview_person(&mut connection, ws, template.id, saved, None)
                .await
                .unwrap();

        assert!(created && !created_again);
        assert_eq!(first, again);
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM template_preview_people")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_template_keeps_at_most_twenty_people_and_an_unknown_person_is_refused(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let template = create(&pool, ws, "Welcome").await;
        let other = create(&pool, ws, "Other").await;
        let mut connection = pool.acquire().await.unwrap();
        let mut last = Uuid::nil();
        for index in 0..MAX_PREVIEW_PEOPLE {
            last = person(&pool, ws, &format!("p{index}")).await;
            add_preview_person(&mut connection, ws, template.id, last, None)
                .await
                .unwrap();
        }
        let extra = person(&pool, ws, "extra").await;

        let over = add_preview_person(&mut connection, ws, template.id, extra, None).await;
        let again = add_preview_person(&mut connection, ws, template.id, last, None).await;
        let elsewhere = add_preview_person(&mut connection, ws, other.id, extra, None).await;
        let unknown = add_preview_person(&mut connection, ws, other.id, Uuid::now_v7(), None).await;

        assert!(matches!(over, Err(PreviewPersonError::Limit)));
        assert!(matches!(again, Ok((_, false))));
        assert!(elsewhere.is_ok());
        assert!(matches!(unknown, Err(PreviewPersonError::NotFound)));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_add_that_waits_for_a_merge_lands_on_the_survivor(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let template = create(&pool, ws, "Welcome").await;
        let (survivor, loser) = (person(&pool, ws, "s").await, person(&pool, ws, "l").await);

        // The merge, held open: it locks the group as the real one does, points the loser at the
        // survivor and moves the saved rows.
        let mut merge = crate::begin(&pool).await.unwrap();
        sqlx::query("SELECT id FROM people WHERE id = ANY($1) ORDER BY id FOR UPDATE")
            .bind(vec![survivor, loser])
            .fetch_all(&mut *merge)
            .await
            .unwrap();
        sqlx::query("UPDATE people SET merged_into = $2 WHERE id = $1")
            .bind(loser)
            .bind(survivor)
            .execute(&mut *merge)
            .await
            .unwrap();
        move_to_survivor(&mut merge, ws, &[loser], survivor)
            .await
            .unwrap();

        let adding = {
            let pool = pool.clone();
            let template = template.id;
            tokio::spawn(async move {
                let mut tx = crate::begin(&pool).await.unwrap();
                let added = add_preview_person(&mut tx, ws, template, loser, None)
                    .await
                    .unwrap();
                crate::commit(tx).await.unwrap();
                added
            })
        };
        // Wait until the add is blocked on the merge's lock.
        for attempt in 0..500 {
            assert!(attempt < 499, "the add never waited for the merge");
            let blocked: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE datname = current_database() AND wait_event_type = 'Lock'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if blocked > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        crate::commit(merge).await.unwrap();

        let (saved, created) = adding.await.unwrap();

        assert!(created);
        assert_eq!(saved.person_id, survivor);
        let rows: Vec<Uuid> = sqlx::query_scalar("SELECT person_id FROM template_preview_people")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(rows, [survivor]);
    }
}
