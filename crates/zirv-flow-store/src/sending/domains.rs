//! Sending domains and their records: create, read, list and delete. Every function is scoped to
//! one workspace.

use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::sending::{RecordKind, dkim_record_text, dmarc_record_text, spf_record_text};
use zirv_flow_core::{SecretId, UserId, WorkspaceId};

use crate::StoreError;

/// A sending domain. The private key is a reference to a sealed secret, never a value.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct SendingDomain {
    pub id: Uuid,
    pub domain: String,
    pub selector: String,
    pub dkim_secret_id: Uuid,
    /// The base64 SubjectPublicKeyInfo of the DKIM key: the `p=` value.
    pub dkim_public_key: String,
    /// `unverified` or `verified`.
    pub status: String,
    pub verified_at: Option<OffsetDateTime>,
    pub last_checked_at: Option<OffsetDateTime>,
    pub created_by: Option<Uuid>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

pub(super) const VERIFIED: &str = "verified";
pub(super) const UNVERIFIED: &str = "unverified";

macro_rules! domain_columns {
    () => {
        "id, domain, selector, dkim_secret_id, dkim_public_key, status, verified_at, \
         last_checked_at, created_by, created_at, updated_at"
    };
}
pub(super) use domain_columns;

/// One of a domain's three records and what the last check found.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct DomainRecord {
    /// `dkim`, `spf` or `dmarc`.
    pub kind: String,
    /// The recommended text to publish.
    pub expected_text: String,
    /// `pending`, `ok`, `mismatch` or `missing`.
    pub status: String,
    /// The TXT records the last check read at the record's name.
    pub observed_text: Vec<String>,
    pub checked_at: Option<OffsetDateTime>,
}

macro_rules! record_columns {
    () => {
        "kind, expected_text, status, observed_text, checked_at"
    };
}

/// A domain to create. The secret named by `dkim_secret_id` must already exist in the same
/// transaction.
#[derive(Debug)]
pub struct NewSendingDomain<'a> {
    pub id: Uuid,
    /// Normalized by `zirv_flow_core::sending::parse_domain`.
    pub domain: &'a str,
    pub selector: &'a str,
    pub dkim_secret_id: SecretId,
    pub dkim_public_key: &'a str,
    /// The SPF mechanism the domain's record must contain; none accepts any SPF record.
    pub spf_mechanism: Option<&'a str>,
    pub created_by: Option<UserId>,
}

/// Creates an unverified domain and its three pending records. A domain the workspace already
/// has is a conflict. Run it in a transaction with the secret insert.
pub async fn create_sending_domain(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    new: &NewSendingDomain<'_>,
) -> Result<(SendingDomain, Vec<DomainRecord>), StoreError> {
    let domain: SendingDomain = sqlx::query_as(concat!(
        "INSERT INTO sending_domains (id, workspace_id, domain, selector, dkim_secret_id, \
            dkim_public_key, created_by) VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING ",
        domain_columns!()
    ))
    .bind(new.id)
    .bind(workspace.as_uuid())
    .bind(new.domain)
    .bind(new.selector)
    .bind(new.dkim_secret_id.as_uuid())
    .bind(new.dkim_public_key)
    .bind(new.created_by.map(|user| user.as_uuid()))
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let texts = [
        (RecordKind::Dkim, dkim_record_text(new.dkim_public_key)),
        (RecordKind::Spf, spf_record_text(new.spf_mechanism)),
        (RecordKind::Dmarc, dmarc_record_text()),
    ];
    for (kind, text) in &texts {
        sqlx::query(
            "INSERT INTO sending_domain_records (workspace_id, domain_id, kind, expected_text) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(workspace.as_uuid())
        .bind(domain.id)
        .bind(kind.as_str())
        .bind(text)
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    }
    let records = list_domain_records(&mut *connection, workspace, domain.id).await?;
    Ok((domain, records))
}

/// The workspace's domains, ordered by name.
pub async fn list_sending_domains<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
) -> Result<Vec<SendingDomain>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        domain_columns!(),
        " FROM sending_domains WHERE workspace_id = $1 ORDER BY domain, id"
    ))
    .bind(workspace.as_uuid())
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// One domain of the workspace.
pub async fn find_sending_domain<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<SendingDomain>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        domain_columns!(),
        " FROM sending_domains WHERE workspace_id = $1 AND id = $2"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// A domain's records in the order DKIM, SPF, DMARC.
pub async fn list_domain_records<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    domain: Uuid,
) -> Result<Vec<DomainRecord>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        record_columns!(),
        " FROM sending_domain_records WHERE workspace_id = $1 AND domain_id = $2 \
         ORDER BY array_position(ARRAY['dkim', 'spf', 'dmarc'], kind)"
    ))
    .bind(workspace.as_uuid())
    .bind(domain)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

#[derive(FromRow)]
struct WorkspaceRecord {
    domain_id: Uuid,
    #[sqlx(flatten)]
    record: DomainRecord,
}

/// Every record of every domain of the workspace, tagged with its domain id, each domain's
/// records in the order DKIM, SPF, DMARC: one query for a listing, not one per domain.
pub async fn list_workspace_domain_records<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
) -> Result<Vec<(Uuid, DomainRecord)>, StoreError> {
    let rows: Vec<WorkspaceRecord> = sqlx::query_as(concat!(
        "SELECT domain_id, ",
        record_columns!(),
        " FROM sending_domain_records WHERE workspace_id = $1 \
         ORDER BY domain_id, array_position(ARRAY['dkim', 'spf', 'dmarc'], kind)"
    ))
    .bind(workspace.as_uuid())
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(rows
        .into_iter()
        .map(|row| (row.domain_id, row.record))
        .collect())
}

/// What deleting a domain did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DomainDelete {
    /// The domain, its records and its archived identities are gone. The caller deletes the
    /// sealed key `secret` in the same transaction.
    Deleted { secret: SecretId },
    /// The workspace has no such domain.
    NotFound,
    /// A live sender identity or a template that names one of its identities still exists.
    InUse,
}

/// Deletes a domain that nothing uses. Locks the domain row first, so a concurrent identity
/// insert waits and then fails on the foreign key.
pub async fn delete_sending_domain(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<DomainDelete, StoreError> {
    let locked: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM sending_domains WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    if locked.is_none() {
        return Ok(DomainDelete::NotFound);
    }
    let in_use: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM sender_identities i \
             WHERE i.workspace_id = $1 AND i.domain_id = $2 \
               AND (i.archived_at IS NULL \
                    OR EXISTS ( \
                        SELECT 1 FROM templates t \
                        WHERE t.workspace_id = i.workspace_id AND t.sender_identity_id = i.id) \
                    OR EXISTS ( \
                        SELECT 1 FROM templates t \
                        JOIN template_versions v ON v.workspace_id = t.workspace_id \
                            AND v.template_id = t.id AND v.version = t.current_version \
                        WHERE t.workspace_id = i.workspace_id AND v.sender_identity_id = i.id)))",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    if in_use {
        return Ok(DomainDelete::InUse);
    }
    let secret: Uuid = sqlx::query_scalar(
        "DELETE FROM sending_domains WHERE workspace_id = $1 AND id = $2 \
         RETURNING dkim_secret_id",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    Ok(DomainDelete::Deleted {
        secret: SecretId::from_uuid(secret),
    })
}

/// The domains due a DNS check, least recently checked first: `(workspace, domain)`. At most
/// `limit`.
///
/// A domain is due when its last check is older than `older_than_secs` seconds (or never
/// happened), it has no check queued or running, and no check was queued for it within the last
/// `retry_backoff_secs`. The last two keep the same domains from filling every batch: one with a
/// check in flight is skipped, and one whose checks keep failing (a DNS outage stamps no check
/// time) waits out the back-off, so a domain past the first `limit` is still reached.
pub async fn due_sending_domains<'e>(
    executor: impl PgExecutor<'e>,
    older_than_secs: f64,
    retry_backoff_secs: f64,
    limit: i64,
) -> Result<Vec<(WorkspaceId, Uuid)>, StoreError> {
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT d.workspace_id, d.id FROM sending_domains d \
         WHERE (d.last_checked_at IS NULL \
                OR d.last_checked_at < now() - make_interval(secs => $1)) \
           AND (d.last_attempt_at IS NULL \
                OR d.last_attempt_at < now() - make_interval(secs => $2)) \
           AND NOT EXISTS ( \
                SELECT 1 FROM jobs j \
                WHERE j.unique_key = 'sending_domain.verify:' || d.id::text \
                  AND j.state IN ('ready', 'running')) \
         ORDER BY d.last_checked_at NULLS FIRST, d.id LIMIT $3",
    )
    .bind(older_than_secs)
    .bind(retry_backoff_secs)
    .bind(limit)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(rows
        .into_iter()
        .map(|(workspace, id)| (WorkspaceId::from_uuid(workspace), id))
        .collect())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::sending::fixtures::{plant_domain, plant_identity};
    use crate::templates::tests_support::workspace;

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_domain_starts_unverified_with_three_pending_records_in_order(pool: PgPool) {
        let ws = workspace(&pool, "one").await;

        let id = plant_domain(&pool, ws, "example.com").await.unwrap();

        let domain = find_sending_domain(&pool, ws, id).await.unwrap().unwrap();
        assert_eq!(domain.status, "unverified");
        assert!(domain.verified_at.is_none() && domain.last_checked_at.is_none());
        let records = list_domain_records(&pool, ws, id).await.unwrap();
        let kinds: Vec<_> = records.iter().map(|record| record.kind.as_str()).collect();
        assert_eq!(kinds, ["dkim", "spf", "dmarc"]);
        assert!(records.iter().all(|record| record.status == "pending"));
        assert!(records[0].expected_text.starts_with("v=DKIM1; k=rsa; p="));
        assert_eq!(records[1].expected_text, "v=spf1 ~all");
        assert_eq!(records[2].expected_text, "v=DMARC1; p=none");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_workspace_listing_of_records_matches_the_per_domain_one(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;
        let first = plant_domain(&pool, ws, "a.example.com").await.unwrap();
        let second = plant_domain(&pool, ws, "b.example.com").await.unwrap();
        plant_domain(&pool, other, "c.example.com").await.unwrap();

        let all = list_workspace_domain_records(&pool, ws).await.unwrap();

        assert_eq!(all.len(), 6);
        for id in [first, second] {
            let of_domain: Vec<_> = all
                .iter()
                .filter(|(domain, _)| *domain == id)
                .map(|(_, record)| record.clone())
                .collect();
            assert_eq!(of_domain, list_domain_records(&pool, ws, id).await.unwrap());
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_workspace_registers_a_domain_once_but_two_workspaces_may_share_one(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        plant_domain(&pool, one, "example.com").await.unwrap();

        let again = plant_domain(&pool, one, "example.com").await;

        assert!(again.unwrap_err().is_conflict());
        plant_domain(&pool, two, "example.com").await.unwrap();
        assert_eq!(list_sending_domains(&pool, one).await.unwrap().len(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn another_workspace_does_not_see_the_domain(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let id = plant_domain(&pool, one, "example.com").await.unwrap();

        assert!(find_sending_domain(&pool, two, id).await.unwrap().is_none());
        assert!(
            list_domain_records(&pool, two, id)
                .await
                .unwrap()
                .is_empty()
        );
        let mut connection = pool.acquire().await.unwrap();
        assert_eq!(
            delete_sending_domain(&mut connection, two, id)
                .await
                .unwrap(),
            DomainDelete::NotFound
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_domain_with_a_live_identity_cannot_be_deleted_and_an_archived_one_goes_with_it(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let id = plant_domain(&pool, ws, "example.com").await.unwrap();
        let identity = plant_identity(&pool, ws, id, "news@example.com")
            .await
            .unwrap();
        let mut connection = pool.acquire().await.unwrap();

        assert_eq!(
            delete_sending_domain(&mut connection, ws, id)
                .await
                .unwrap(),
            DomainDelete::InUse
        );
        crate::archive_sender_identity(&pool, ws, identity)
            .await
            .unwrap();
        let outcome = delete_sending_domain(&mut connection, ws, id)
            .await
            .unwrap();

        assert!(matches!(outcome, DomainDelete::Deleted { .. }));
        assert!(find_sending_domain(&pool, ws, id).await.unwrap().is_none());
        assert!(list_domain_records(&pool, ws, id).await.unwrap().is_empty());
        let identities: i64 = sqlx::query_scalar("SELECT count(*) FROM sender_identities")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(identities, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_domain_whose_archived_identity_a_template_names_cannot_be_deleted(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let id = plant_domain(&pool, ws, "example.com").await.unwrap();
        let identity = plant_identity(&pool, ws, id, "news@example.com")
            .await
            .unwrap();
        let template = crate::templates::tests_support::create(&pool, ws, "t").await;
        sqlx::query("UPDATE templates SET sender_identity_id = $2 WHERE id = $1")
            .bind(template.id)
            .bind(identity)
            .execute(&pool)
            .await
            .unwrap();
        crate::archive_sender_identity(&pool, ws, identity)
            .await
            .unwrap();
        let mut connection = pool.acquire().await.unwrap();

        let outcome = delete_sending_domain(&mut connection, ws, id)
            .await
            .unwrap();

        assert_eq!(outcome, DomainDelete::InUse);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn due_domains_are_the_never_checked_and_the_stale_ones_oldest_first(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let never = plant_domain(&pool, ws, "never.example.com").await.unwrap();
        let stale = plant_domain(&pool, ws, "stale.example.com").await.unwrap();
        let fresh = plant_domain(&pool, ws, "fresh.example.com").await.unwrap();
        sqlx::query(
            "UPDATE sending_domains SET last_checked_at = now() - interval '7 hours' WHERE id = $1",
        )
        .bind(stale)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE sending_domains SET last_checked_at = now() WHERE id = $1")
            .bind(fresh)
            .execute(&pool)
            .await
            .unwrap();

        let due = due_sending_domains(&pool, 6.0 * 3600.0, 0.0, 10)
            .await
            .unwrap();

        assert_eq!(due, [(ws, never), (ws, stale)]);
        assert_eq!(
            due_sending_domains(&pool, 6.0 * 3600.0, 0.0, 1)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    fn naming(sender: Uuid) -> serde_json::Value {
        serde_json::json!({"schema": 1, "channel": "email", "mode": "code",
                           "email": {"subject": "s", "sender": sender, "html": "<p>x</p>"}})
    }

    async fn publish(pool: &PgPool, ws: WorkspaceId, template: Uuid) {
        let mut tx = crate::begin(pool).await.unwrap();
        let locked = crate::lock_template(&mut tx, ws, template).await.unwrap();
        let trace =
            zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::definition_change(template));
        crate::publish_template(&mut tx, ws, &locked, &serde_json::json!([]), None, &trace)
            .await
            .unwrap();
        crate::commit(tx).await.unwrap();
    }

    async fn draft(pool: &PgPool, ws: WorkspaceId, template: Uuid, document: &serde_json::Value) {
        let mut connection = pool.acquire().await.unwrap();
        crate::save_template_draft(&mut connection, ws, template, document, None)
            .await
            .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_domain_is_in_use_while_a_draft_or_the_current_version_names_its_identity(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let domain = plant_domain(&pool, ws, "example.com").await.unwrap();
        let identity = plant_identity(&pool, ws, domain, "news@example.com")
            .await
            .unwrap();
        let template = crate::templates::tests_support::create(&pool, ws, "t").await;
        let deletable = |pool: PgPool| async move {
            let mut tx = crate::begin(&pool).await.unwrap();
            let outcome = delete_sending_domain(&mut tx, ws, domain).await.unwrap();
            // Rolled back: this only asks.
            drop(tx);
            outcome
        };

        // A published version names the identity; the draft then moves on.
        draft(&pool, ws, template.id, &naming(identity)).await;
        publish(&pool, ws, template.id).await;
        draft(
            &pool,
            ws,
            template.id,
            &crate::templates::tests_support::document("x"),
        )
        .await;
        crate::archive_sender_identity(&pool, ws, identity)
            .await
            .unwrap();
        let current_version_names_it = deletable(pool.clone()).await;
        assert_eq!(current_version_names_it, DomainDelete::InUse);

        // A draft names it (the version no longer is the current one).
        publish(&pool, ws, template.id).await;
        draft(&pool, ws, template.id, &naming(identity)).await;
        assert_eq!(deletable(pool.clone()).await, DomainDelete::InUse);

        // Only an old version names it: the delete goes ahead.
        draft(
            &pool,
            ws,
            template.id,
            &crate::templates::tests_support::document("y"),
        )
        .await;
        assert!(matches!(
            deletable(pool.clone()).await,
            DomainDelete::Deleted { .. }
        ));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_new_tables_hold_no_person_column_for_the_erasure_scan(pool: PgPool) {
        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT table_name::text || '.' || column_name::text \
             FROM information_schema.columns \
             WHERE table_schema = current_schema() \
               AND table_name IN ('sending_domains', 'sending_domain_records', 'sender_identities') \
               AND (column_name = 'person_id' OR column_name LIKE '%\\_person\\_id' \
                    OR column_name ~ '(email|phone|recipient|\\mto\\M)') \
             ORDER BY 1",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        assert!(columns.is_empty(), "{columns:?}");
        // The scan the privacy tests run names person columns the same way.
        let scan: Vec<String> = sqlx::query_scalar(
            "SELECT table_name::text FROM information_schema.columns \
             WHERE table_schema = current_schema() AND data_type = 'uuid' \
               AND (column_name = 'person_id' OR column_name LIKE '%\\_person\\_id') \
               AND table_name IN ('sending_domains', 'sending_domain_records', 'sender_identities')",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(scan.is_empty(), "{scan:?}");
    }
}
