//! Sender identities: a from name and address on one sending domain. The address must be on the
//! identity's domain; the store checks that when it writes (a CHECK cannot look at another row).

use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::sending::address_domain;
use zirv_flow_core::{Email, UserId, WorkspaceId};

use crate::StoreError;

/// A sender identity.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct SenderIdentity {
    pub id: Uuid,
    pub domain_id: Uuid,
    pub from_name: String,
    pub from_address: String,
    pub reply_to: Option<String>,
    pub archived_at: Option<OffsetDateTime>,
    pub created_by: Option<Uuid>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

macro_rules! identity_columns {
    () => {
        "id, domain_id, from_name, from_address, reply_to, archived_at, created_by, created_at, \
         updated_at"
    };
}

/// Why an identity write was refused.
#[derive(Debug)]
pub enum IdentityError {
    /// The workspace has no such domain, or no such live identity.
    NotFound,
    /// The from address is not on the identity's domain.
    OffDomain,
    /// A live identity already has the from address.
    Taken,
    Store(StoreError),
}

impl From<StoreError> for IdentityError {
    fn from(error: StoreError) -> Self {
        if error.is_conflict() {
            Self::Taken
        } else {
            Self::Store(error)
        }
    }
}

/// An identity to create.
#[derive(Debug)]
pub struct NewSenderIdentity<'a> {
    pub id: Uuid,
    pub domain_id: Uuid,
    pub from_name: &'a str,
    pub from_address: &'a Email,
    pub reply_to: Option<&'a Email>,
    pub created_by: Option<UserId>,
}

/// The fields of a live identity that can change. The domain does not: an identity belongs to
/// the domain it was made on.
#[derive(Debug, Default)]
pub struct SenderIdentityPatch<'a> {
    pub from_name: Option<&'a str>,
    pub from_address: Option<&'a Email>,
    /// `Some(None)` clears the reply-to address.
    pub reply_to: Option<Option<&'a Email>>,
}

/// Reads the domain name of `domain_id`, holding its row `FOR SHARE` so it cannot be deleted
/// before the identity row that refers to it is written.
async fn domain_name(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    domain_id: Uuid,
) -> Result<Option<String>, StoreError> {
    sqlx::query_scalar(
        "SELECT domain FROM sending_domains WHERE workspace_id = $1 AND id = $2 FOR SHARE",
    )
    .bind(workspace.as_uuid())
    .bind(domain_id)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)
}

fn on_domain(address: &Email, domain: &str) -> bool {
    address_domain(address.as_str()) == Some(domain)
}

/// Creates an identity on a domain of the workspace. The from address must be on the domain.
pub async fn create_sender_identity(
    pool: &PgPool,
    workspace: WorkspaceId,
    new: &NewSenderIdentity<'_>,
) -> Result<SenderIdentity, IdentityError> {
    let mut tx = crate::begin(pool).await?;
    let identity = create_sender_identity_in(&mut tx, workspace, new).await?;
    crate::commit(tx).await?;
    Ok(identity)
}

/// As [`create_sender_identity`], in the caller's transaction.
pub async fn create_sender_identity_in(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    new: &NewSenderIdentity<'_>,
) -> Result<SenderIdentity, IdentityError> {
    let domain = domain_name(connection, workspace, new.domain_id)
        .await?
        .ok_or(IdentityError::NotFound)?;
    if !on_domain(new.from_address, &domain) {
        return Err(IdentityError::OffDomain);
    }
    Ok(sqlx::query_as(concat!(
        "INSERT INTO sender_identities (id, workspace_id, domain_id, from_name, from_address, \
            reply_to, created_by) VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING ",
        identity_columns!()
    ))
    .bind(new.id)
    .bind(workspace.as_uuid())
    .bind(new.domain_id)
    .bind(new.from_name)
    .bind(new.from_address.as_str())
    .bind(new.reply_to.map(Email::as_str))
    .bind(new.created_by.map(|user| user.as_uuid()))
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)?)
}

/// The workspace's identities, ordered by address; archived ones only when asked for, and only
/// those of `domain` when one is given.
pub async fn list_sender_identities<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    domain: Option<Uuid>,
    include_archived: bool,
) -> Result<Vec<SenderIdentity>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        identity_columns!(),
        " FROM sender_identities WHERE workspace_id = $1 AND ($2::uuid IS NULL OR domain_id = $2) \
          AND ($3 OR archived_at IS NULL) ORDER BY from_address, id"
    ))
    .bind(workspace.as_uuid())
    .bind(domain)
    .bind(include_archived)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// One identity, archived or not.
pub async fn find_sender_identity<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<SenderIdentity>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        identity_columns!(),
        " FROM sender_identities WHERE workspace_id = $1 AND id = $2"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// Changes the given fields of a live identity. A new from address must be on the identity's
/// domain.
pub async fn update_sender_identity(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    patch: &SenderIdentityPatch<'_>,
) -> Result<SenderIdentity, IdentityError> {
    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT domain_id FROM sender_identities \
         WHERE workspace_id = $1 AND id = $2 AND archived_at IS NULL FOR UPDATE",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let domain_id = existing.ok_or(IdentityError::NotFound)?;
    if let Some(address) = patch.from_address {
        let domain = domain_name(connection, workspace, domain_id)
            .await?
            .ok_or(IdentityError::NotFound)?;
        if !on_domain(address, &domain) {
            return Err(IdentityError::OffDomain);
        }
    }
    Ok(sqlx::query_as(concat!(
        "UPDATE sender_identities SET from_name = COALESCE($3, from_name), \
            from_address = COALESCE($4, from_address), \
            reply_to = CASE WHEN $5 THEN $6 ELSE reply_to END, updated_at = now() \
         WHERE workspace_id = $1 AND id = $2 RETURNING ",
        identity_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(patch.from_name)
    .bind(patch.from_address.map(Email::as_str))
    .bind(patch.reply_to.is_some())
    .bind(patch.reply_to.flatten().map(Email::as_str))
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)?)
}

/// Archives a live identity and returns it; `None` when there is no such live identity. Its
/// address is free again; published template versions keep naming it and are refused as an
/// unverified sender when sent.
pub async fn archive_sender_identity<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<SenderIdentity>, StoreError> {
    sqlx::query_as(concat!(
        "UPDATE sender_identities SET archived_at = now(), updated_at = now() \
         WHERE workspace_id = $1 AND id = $2 AND archived_at IS NULL RETURNING ",
        identity_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sending::fixtures::plant_domain;
    use crate::templates::tests_support::workspace;

    fn address(text: &str) -> Email {
        Email::parse(text).unwrap()
    }

    fn new<'a>(domain: Uuid, address: &'a Email) -> NewSenderIdentity<'a> {
        NewSenderIdentity {
            id: Uuid::now_v7(),
            domain_id: domain,
            from_name: "News",
            from_address: address,
            reply_to: None,
            created_by: None,
        }
    }

    #[test]
    fn an_address_is_on_a_domain_only_when_it_ends_with_exactly_that_domain() {
        assert!(on_domain(&address("a@example.com"), "example.com"));
        assert!(!on_domain(&address("a@mail.example.com"), "example.com"));
        assert!(!on_domain(&address("a@badexample.com"), "example.com"));
        assert!(!on_domain(
            &address("a@example.com.evil.net"),
            "example.com"
        ));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_identity_off_its_domain_is_refused_and_nothing_is_written(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let domain = plant_domain(&pool, ws, "example.com").await.unwrap();

        for bad in ["a@other.com", "a@mail.example.com", "a@badexample.com"] {
            let bad = address(bad);
            let refused = create_sender_identity(&pool, ws, &new(domain, &bad)).await;
            assert!(matches!(refused, Err(IdentityError::OffDomain)), "{bad}");
        }
        assert!(
            list_sender_identities(&pool, ws, None, true)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_database_keeps_the_shape_of_an_address_the_store_would_have_refused(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let domain = plant_domain(&pool, ws, "example.com").await.unwrap();

        let refused = sqlx::query(
            "INSERT INTO sender_identities (id, workspace_id, domain_id, from_name, from_address) \
             VALUES ($1, $2, $3, 'N', 'Not-Lower@example.com')",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .bind(domain)
        .execute(&pool)
        .await;

        assert!(refused.is_err());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_identity_on_another_workspaces_domain_is_not_found(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let domain = plant_domain(&pool, one, "example.com").await.unwrap();
        let from = address("a@example.com");

        let refused = create_sender_identity(&pool, two, &new(domain, &from)).await;

        assert!(matches!(refused, Err(IdentityError::NotFound)));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_live_address_is_unique_and_free_again_once_archived(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let domain = plant_domain(&pool, ws, "example.com").await.unwrap();
        let from = address("News@Example.com");
        let first = create_sender_identity(&pool, ws, &new(domain, &from))
            .await
            .unwrap();
        assert_eq!(first.from_address, "news@example.com");

        let second = create_sender_identity(&pool, ws, &new(domain, &from)).await;
        assert!(matches!(second, Err(IdentityError::Taken)));
        let archived = archive_sender_identity(&pool, ws, first.id)
            .await
            .unwrap()
            .unwrap();
        assert!(archived.archived_at.is_some());
        assert!(
            archive_sender_identity(&pool, ws, first.id)
                .await
                .unwrap()
                .is_none()
        );
        create_sender_identity(&pool, ws, &new(domain, &from))
            .await
            .unwrap();

        assert_eq!(
            list_sender_identities(&pool, ws, None, false)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            list_sender_identities(&pool, ws, Some(domain), true)
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(
            list_sender_identities(&pool, ws, Some(Uuid::now_v7()), true)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_update_changes_the_given_fields_and_keeps_the_address_on_the_domain(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let domain = plant_domain(&pool, ws, "example.com").await.unwrap();
        let from = address("news@example.com");
        let identity = create_sender_identity(&pool, ws, &new(domain, &from))
            .await
            .unwrap();
        let mut connection = pool.acquire().await.unwrap();
        let reply = address("help@elsewhere.org");

        let updated = update_sender_identity(
            &mut connection,
            ws,
            identity.id,
            &SenderIdentityPatch {
                from_name: Some("Newsroom"),
                reply_to: Some(Some(&reply)),
                ..SenderIdentityPatch::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(updated.from_name, "Newsroom");
        assert_eq!(updated.reply_to.as_deref(), Some("help@elsewhere.org"));
        assert_eq!(updated.from_address, "news@example.com");

        let cleared = update_sender_identity(
            &mut connection,
            ws,
            identity.id,
            &SenderIdentityPatch {
                reply_to: Some(None),
                ..SenderIdentityPatch::default()
            },
        )
        .await
        .unwrap();
        assert!(cleared.reply_to.is_none());

        let off = address("news@other.com");
        let refused = update_sender_identity(
            &mut connection,
            ws,
            identity.id,
            &SenderIdentityPatch {
                from_address: Some(&off),
                ..SenderIdentityPatch::default()
            },
        )
        .await;
        assert!(matches!(refused, Err(IdentityError::OffDomain)));
        let missing = update_sender_identity(
            &mut connection,
            ws,
            Uuid::now_v7(),
            &SenderIdentityPatch::default(),
        )
        .await;
        assert!(matches!(missing, Err(IdentityError::NotFound)));
    }
}
