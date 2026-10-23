//! The shared refusal for a sender that may not send: unknown, archived, or on a domain whose
//! DNS records are not all in place. Template publish and, later, the send path both call it, so
//! the rule lives in one place.

use sqlx::PgConnection;
use sqlx::prelude::FromRow;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::StoreError;

/// A sender identity on a verified domain: what a send needs to address the message.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct VerifiedSender {
    pub identity_id: Uuid,
    pub domain_id: Uuid,
    pub domain: String,
    pub from_name: String,
    pub from_address: String,
    pub reply_to: Option<String>,
}

/// Why a sender may not send. The API answers each with `sender_unverified` (422).
#[derive(Debug)]
pub enum SenderError {
    /// The workspace has no such sender identity.
    Unknown,
    /// The identity is archived.
    Archived,
    /// The identity's domain is not verified: a record is missing or wrong, or was last found so.
    Unverified {
        domain: String,
    },
    Store(StoreError),
}

impl From<StoreError> for SenderError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

#[derive(FromRow)]
struct Row {
    identity_id: Uuid,
    domain_id: Uuid,
    from_name: String,
    from_address: String,
    reply_to: Option<String>,
    archived: bool,
}

/// The sender identity `identity` of the workspace when it is live and its domain is verified.
///
/// Both rows are read `FOR SHARE`: run inside the transaction that relies on the answer, an
/// archive of the identity or a check that unverifies the domain waits for that transaction to end,
/// so a publish never commits a sender that stopped being valid under it. Outside a transaction
/// the answer is only as of the read.
///
/// Lock order, here and on every path that holds both rows: the domain first, then the identity.
/// The domain is found from the identity without a lock, locked, and the identity locked in a
/// second statement, so the order does not depend on a join plan. Deleting a domain locks the
/// domain and then cascades to its identities, which is the same order, so a check and a delete
/// wait for one another instead of deadlocking.
pub async fn require_verified_sender(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    identity: Uuid,
) -> Result<VerifiedSender, SenderError> {
    let domain_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT domain_id FROM sender_identities WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(identity)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let domain_id = domain_id.ok_or(SenderError::Unknown)?;
    // The domain, `FOR SHARE`. A domain deleted meanwhile takes its identities with it.
    let domain: Option<(String, bool)> = sqlx::query_as(
        "SELECT domain, status = 'verified' FROM sending_domains \
         WHERE workspace_id = $1 AND id = $2 FOR SHARE",
    )
    .bind(workspace.as_uuid())
    .bind(domain_id)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let (domain, verified) = domain.ok_or(SenderError::Unknown)?;
    // The identity, `FOR SHARE`, after the domain.
    let row: Option<Row> = sqlx::query_as(
        "SELECT id AS identity_id, domain_id, from_name, from_address, reply_to, \
                archived_at IS NOT NULL AS archived \
         FROM sender_identities WHERE workspace_id = $1 AND id = $2 FOR SHARE",
    )
    .bind(workspace.as_uuid())
    .bind(identity)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let row = row.ok_or(SenderError::Unknown)?;
    if row.archived {
        return Err(SenderError::Archived);
    }
    if !verified {
        return Err(SenderError::Unverified { domain });
    }
    Ok(VerifiedSender {
        identity_id: row.identity_id,
        domain_id: row.domain_id,
        domain,
        from_name: row.from_name,
        from_address: row.from_address,
        reply_to: row.reply_to,
    })
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::sending::fixtures::{
        plant_domain, plant_identity, plant_verified_sender, verify_domain,
    };
    use crate::templates::tests_support::workspace;

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_live_identity_on_a_verified_domain_is_returned_with_its_address(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let identity = plant_verified_sender(&pool, ws, "example.com")
            .await
            .unwrap();

        let sender = check(&pool, ws, identity).await.unwrap();

        assert_eq!(sender.identity_id, identity);
        assert_eq!(sender.from_address, "news@example.com");
        assert_eq!(sender.domain, "example.com");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_identity_on_an_unverified_domain_is_refused_until_it_verifies(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let domain = plant_domain(&pool, ws, "example.com").await.unwrap();
        let identity = plant_identity(&pool, ws, domain, "news@example.com")
            .await
            .unwrap();

        let refused = check(&pool, ws, identity).await;
        assert!(
            matches!(refused, Err(SenderError::Unverified { ref domain }) if domain == "example.com")
        );
        verify_domain(&pool, ws, domain).await.unwrap();

        assert!(check(&pool, ws, identity).await.is_ok());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_unknown_archived_or_foreign_identity_is_refused(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let identity = plant_verified_sender(&pool, one, "example.com")
            .await
            .unwrap();

        assert!(matches!(
            check(&pool, one, Uuid::now_v7()).await,
            Err(SenderError::Unknown)
        ));
        assert!(matches!(
            check(&pool, two, identity).await,
            Err(SenderError::Unknown)
        ));
        crate::archive_sender_identity(&pool, one, identity)
            .await
            .unwrap();
        assert!(matches!(
            check(&pool, one, identity).await,
            Err(SenderError::Archived)
        ));
    }

    /// Runs `statement` in a second transaction with a short lock timeout; true when it was
    /// blocked by a lock held elsewhere.
    async fn blocked(pool: &PgPool, statement: &str, bind: Uuid) -> bool {
        let mut tx = crate::begin(pool).await.unwrap();
        sqlx::query("SET LOCAL lock_timeout = '300ms'")
            .execute(&mut *tx)
            .await
            .unwrap();
        let result = sqlx::query(sqlx::AssertSqlSafe(statement.to_owned()))
            .bind(bind)
            .execute(&mut *tx)
            .await;
        result.is_err()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_check_holds_the_identity_and_the_domain_until_its_transaction_ends(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let identity = plant_verified_sender(&pool, ws, "example.com")
            .await
            .unwrap();
        let domain: Uuid = sqlx::query_scalar("SELECT domain_id FROM sender_identities")
            .fetch_one(&pool)
            .await
            .unwrap();
        let archive = "UPDATE sender_identities SET archived_at = now() WHERE id = $1";
        let unverify = "SELECT id FROM sending_domains WHERE id = $1 FOR UPDATE";

        let mut open = crate::begin(&pool).await.unwrap();
        require_verified_sender(&mut open, ws, identity)
            .await
            .unwrap();
        assert!(blocked(&pool, archive, identity).await, "the archive waits");
        assert!(blocked(&pool, unverify, domain).await, "the unverify waits");
        crate::commit(open).await.unwrap();

        assert!(!blocked(&pool, unverify, domain).await);
        crate::archive_sender_identity(&pool, ws, identity)
            .await
            .unwrap()
            .unwrap();
    }

    async fn check(
        pool: &PgPool,
        ws: zirv_flow_core::WorkspaceId,
        identity: Uuid,
    ) -> Result<VerifiedSender, SenderError> {
        let mut connection = pool.acquire().await.unwrap();
        require_verified_sender(&mut connection, ws, identity).await
    }

    /// The SQLSTATE of a statement that failed.
    fn state(error: &sqlx::Error) -> String {
        error
            .as_database_error()
            .and_then(|error| error.code())
            .map(|code| code.to_string())
            .unwrap_or_default()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_check_and_a_domain_delete_wait_for_one_another_in_either_order_and_never_deadlock(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let identity = plant_verified_sender(&pool, ws, "example.com")
            .await
            .unwrap();
        let domain: Uuid = sqlx::query_scalar("SELECT domain_id FROM sender_identities")
            .fetch_one(&pool)
            .await
            .unwrap();
        let lock_timeout = "SET LOCAL lock_timeout = '300ms'";

        // A checks, B deletes the domain: B waits (lock not available, 55P03), no deadlock.
        let mut a = crate::begin(&pool).await.unwrap();
        require_verified_sender(&mut a, ws, identity).await.unwrap();
        let mut b = crate::begin(&pool).await.unwrap();
        sqlx::query(lock_timeout).execute(&mut *b).await.unwrap();
        let waited = sqlx::query("DELETE FROM sending_domains WHERE id = $1")
            .bind(domain)
            .execute(&mut *b)
            .await
            .unwrap_err();
        assert_eq!(state(&waited), "55P03", "{waited}");
        drop(b);
        crate::commit(a).await.unwrap();

        // B deletes first (domain locked, cascade into the identity), A checks: A waits.
        let mut b = crate::begin(&pool).await.unwrap();
        crate::delete_sending_domain(&mut b, ws, domain)
            .await
            .unwrap();
        let mut a = crate::begin(&pool).await.unwrap();
        sqlx::query(lock_timeout).execute(&mut *a).await.unwrap();
        let waited = require_verified_sender(&mut a, ws, identity).await;
        match waited {
            Err(SenderError::Store(error)) => {
                let source = std::error::Error::source(&error)
                    .and_then(|source| source.downcast_ref::<sqlx::Error>())
                    .map(state)
                    .unwrap_or_default();
                assert_eq!(source, "55P03");
            }
            other => panic!("the check should wait for the delete: {other:?}"),
        }
    }
}
