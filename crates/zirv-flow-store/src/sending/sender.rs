//! The shared refusal for a sender that may not send: unknown, archived, or on a domain whose
//! DNS records are not all in place. Template publish and, later, the send path both call it, so
//! the rule lives in one place.

use sqlx::PgExecutor;
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
    domain: String,
    from_name: String,
    from_address: String,
    reply_to: Option<String>,
    archived: bool,
    verified: bool,
}

/// The sender identity `identity` of the workspace when it is live and its domain is verified.
///
/// Both rows are read `FOR SHARE`: run inside the transaction that relies on the answer, an
/// archive of the identity or a check that unverifies the domain waits for that transaction to end,
/// so a publish never commits a sender that stopped being valid under it. Outside a transaction
/// the answer is only as of the read.
pub async fn require_verified_sender<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    identity: Uuid,
) -> Result<VerifiedSender, SenderError> {
    let row: Option<Row> = sqlx::query_as(
        "SELECT i.id AS identity_id, d.id AS domain_id, d.domain, i.from_name, i.from_address, \
                i.reply_to, i.archived_at IS NOT NULL AS archived, d.status = 'verified' AS verified \
         FROM sender_identities i \
         JOIN sending_domains d ON d.workspace_id = i.workspace_id AND d.id = i.domain_id \
         WHERE i.workspace_id = $1 AND i.id = $2 FOR SHARE OF i, d",
    )
    .bind(workspace.as_uuid())
    .bind(identity)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    let row = row.ok_or(SenderError::Unknown)?;
    if row.archived {
        return Err(SenderError::Archived);
    }
    if !row.verified {
        return Err(SenderError::Unverified { domain: row.domain });
    }
    Ok(VerifiedSender {
        identity_id: row.identity_id,
        domain_id: row.domain_id,
        domain: row.domain,
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

        let sender = require_verified_sender(&pool, ws, identity).await.unwrap();

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

        let refused = require_verified_sender(&pool, ws, identity).await;
        assert!(
            matches!(refused, Err(SenderError::Unverified { ref domain }) if domain == "example.com")
        );
        verify_domain(&pool, ws, domain).await.unwrap();

        assert!(require_verified_sender(&pool, ws, identity).await.is_ok());
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
            require_verified_sender(&pool, one, Uuid::now_v7()).await,
            Err(SenderError::Unknown)
        ));
        assert!(matches!(
            require_verified_sender(&pool, two, identity).await,
            Err(SenderError::Unknown)
        ));
        crate::archive_sender_identity(&pool, one, identity)
            .await
            .unwrap();
        assert!(matches!(
            require_verified_sender(&pool, one, identity).await,
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
        require_verified_sender(&mut *open, ws, identity)
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
}
