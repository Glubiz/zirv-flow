//! Reading contacts (architecture section 7.6). A contact is a person with a live native
//! source record, so every read is a read of the ledger.

use serde_json::{Map, Value};
use sqlx::PgConnection;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::contacts::{SOURCE, STREAM};
use zirv_flow_core::people::Identifier;

use crate::StoreError;

/// A contact as stored.
#[derive(Clone, Debug, PartialEq)]
pub struct Contact {
    pub key: String,
    /// The surviving person the contact's record belongs to.
    pub person_id: Uuid,
    /// The native record's own values: what was written.
    pub fields: Map<String, Value>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

#[derive(FromRow)]
struct ContactRow {
    record_key: String,
    person_id: Uuid,
    traits: Value,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl From<ContactRow> for Contact {
    fn from(row: ContactRow) -> Self {
        let fields = row
            .traits
            .as_object()
            .map(|traits| {
                traits
                    .iter()
                    .filter_map(|(key, entry)| Some((key.clone(), entry.get("v")?.clone())))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            key: row.record_key,
            person_id: row.person_id,
            fields,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

/// The live native contact records of a workspace; the caller adds conditions and an order.
/// `$1` is the workspace.
const SELECT: &str = "SELECT s.record_key, COALESCE(p.merged_into, s.person_id) AS person_id, \
        s.traits, s.created_at, s.updated_at \
     FROM source_records s \
     JOIN people p ON p.workspace_id = s.workspace_id AND p.id = s.person_id \
     WHERE s.workspace_id = $1 AND s.source = 'native' AND s.stream = 'contacts' \
       AND s.deleted_at IS NULL";

/// One contact by key.
pub async fn find_contact(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &str,
) -> Result<Option<Contact>, StoreError> {
    debug_assert_eq!((SOURCE, STREAM), ("native", "contacts"));
    let row: Option<ContactRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT} AND s.record_key = $2"
    )))
    .bind(workspace.as_uuid())
    .bind(key)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(row.map(Contact::from))
}

/// A page of contacts ordered by key.
#[derive(Clone, Debug, PartialEq)]
pub struct ContactPage {
    pub contacts: Vec<Contact>,
    /// The key to pass as `after` for the next page; none on the last page.
    pub next_after: Option<String>,
}

/// Up to `limit` contacts with a key after `after`, in key order.
pub async fn list_contacts(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    after: Option<&str>,
    limit: u32,
) -> Result<ContactPage, StoreError> {
    let rows: Vec<ContactRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT} AND ($2::text IS NULL OR s.record_key > $2) \
         ORDER BY s.record_key LIMIT $3"
    )))
    .bind(workspace.as_uuid())
    .bind(after)
    .bind(i64::from(limit) + 1)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    let mut contacts: Vec<Contact> = rows.into_iter().map(Contact::from).collect();
    let more = contacts.len() > limit as usize;
    contacts.truncate(limit as usize);
    let next_after = more
        .then(|| contacts.last().map(|contact| contact.key.clone()))
        .flatten();
    Ok(ContactPage {
        contacts,
        next_after,
    })
}

/// What to look contacts up by.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContactSearch {
    /// Contacts whose key starts with this text.
    KeyPrefix(String),
    /// Contacts of the person that holds this identifier, matched exactly after
    /// normalisation.
    Identifier { kind: String, value: String },
}

/// Why a search was refused.
#[derive(Debug)]
pub enum ContactSearchError {
    /// The identifier does not normalise; names the rule, not the value.
    InvalidIdentifier(&'static str),
    Store(StoreError),
}

impl From<StoreError> for ContactSearchError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// Up to `limit` contacts matching `search`, in key order.
pub async fn search_contacts(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    search: &ContactSearch,
    limit: u32,
) -> Result<Vec<Contact>, ContactSearchError> {
    let rows: Vec<ContactRow> = match search {
        ContactSearch::KeyPrefix(prefix) => sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{SELECT} AND starts_with(s.record_key, $2) ORDER BY s.record_key LIMIT $3"
        )))
        .bind(workspace.as_uuid())
        .bind(prefix)
        .bind(i64::from(limit))
        .fetch_all(connection)
        .await
        .map_err(StoreError::query)?,
        ContactSearch::Identifier { kind, value } => {
            let identifier = Identifier::parse(kind, value)
                .map_err(|error| ContactSearchError::InvalidIdentifier(error.reason()))?;
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "{SELECT} AND COALESCE(p.merged_into, p.id) IN ( \
                    SELECT COALESCE(hp.merged_into, hp.id) FROM identities i \
                    JOIN people hp ON hp.workspace_id = i.workspace_id AND hp.id = i.person_id \
                    WHERE i.workspace_id = $1 AND i.kind = $2 AND i.value = $3) \
                 ORDER BY s.record_key LIMIT $4"
            )))
            .bind(workspace.as_uuid())
            .bind(identifier.kind().as_text())
            .bind(identifier.value())
            .bind(i64::from(limit))
            .fetch_all(connection)
            .await
            .map_err(StoreError::query)?
        }
    };
    Ok(rows.into_iter().map(Contact::from).collect())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{defined, object, write};
    use super::super::{ContactChange, ContactSearchError};
    use super::*;
    use serde_json::json;
    use sqlx::PgPool;

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn contacts_are_listed_in_key_order_searched_and_kept_inside_their_workspace(
        pool: PgPool,
    ) {
        let one = defined(&pool, "one").await;
        let two = defined(&pool, "two").await;
        for ws in [one, two] {
            for (key, email) in [
                ("b", "b@example.com"),
                ("a2", "a2@example.com"),
                ("a1", "a1@example.com"),
            ] {
                write(
                    &pool,
                    ws,
                    key,
                    ContactChange::Create(object(json!({"email": email}))),
                    None,
                )
                .await
                .unwrap();
            }
        }
        let mut connection = pool.acquire().await.unwrap();

        let first = list_contacts(&mut connection, one, None, 2).await.unwrap();
        let second = list_contacts(&mut connection, one, first.next_after.as_deref(), 2)
            .await
            .unwrap();
        let prefixed = search_contacts(
            &mut connection,
            one,
            &ContactSearch::KeyPrefix("a".into()),
            10,
        )
        .await
        .unwrap();
        let by_email = search_contacts(
            &mut connection,
            one,
            &ContactSearch::Identifier {
                kind: "email".into(),
                value: "B@Example.com".into(),
            },
            10,
        )
        .await
        .unwrap();
        let invalid = search_contacts(
            &mut connection,
            one,
            &ContactSearch::Identifier {
                kind: "email".into(),
                value: "nope".into(),
            },
            10,
        )
        .await;

        let keys = |contacts: &[Contact]| -> Vec<String> {
            contacts.iter().map(|contact| contact.key.clone()).collect()
        };
        assert_eq!(keys(&first.contacts), ["a1", "a2"]);
        assert_eq!(keys(&second.contacts), ["b"]);
        assert_eq!(second.next_after, None);
        assert_eq!(keys(&prefixed), ["a1", "a2"]);
        assert_eq!(keys(&by_email), ["b"]);
        assert!(matches!(
            invalid,
            Err(ContactSearchError::InvalidIdentifier("invalid_email"))
        ));
        // The same keys in the other workspace belong to other people.
        let other = find_contact(&mut connection, two, "b")
            .await
            .unwrap()
            .unwrap();
        let mine = find_contact(&mut connection, one, "b")
            .await
            .unwrap()
            .unwrap();
        assert_ne!(other.person_id, mine.person_id);
        let theirs = list_contacts(&mut connection, two, None, 10).await.unwrap();
        assert_eq!(keys(&theirs.contacts), ["a1", "a2", "b"]);
    }
}
