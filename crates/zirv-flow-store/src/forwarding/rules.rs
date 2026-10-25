//! Forwarding rules: which accepted events go to which destination, and how they are mapped.
//! Every function is scoped to one workspace.

use serde_json::Value;
use sqlx::PgConnection;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::forwarding::MAX_ACTIVE_RULES;
use zirv_flow_core::{UserId, WorkspaceId};

use crate::StoreError;

/// A stored rule.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct ForwardingRule {
    pub id: Uuid,
    pub destination_id: Uuid,
    pub name: String,
    pub event_name: String,
    /// The segment `where` list; empty selects every event of the name.
    pub filter: Value,
    pub mapping: Value,
    pub active: bool,
    pub created_by: Option<Uuid>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

const COLUMNS: &str = "id, destination_id, name, event_name, filter, mapping, active, created_by, \
                       created_at, updated_at";

/// A rule to create. The caller has validated every field.
#[derive(Debug)]
pub struct NewRule<'a> {
    pub id: Uuid,
    pub destination_id: Uuid,
    pub name: &'a str,
    pub event_name: &'a str,
    pub filter: &'a Value,
    pub mapping: &'a Value,
    pub active: bool,
    pub created_by: Option<UserId>,
}

/// The fields of a rule that can change; `None` leaves a field as it is.
#[derive(Debug, Default)]
pub struct RuleUpdate<'a> {
    pub name: Option<&'a str>,
    pub event_name: Option<&'a str>,
    pub filter: Option<&'a Value>,
    pub mapping: Option<&'a Value>,
    pub active: Option<bool>,
}

/// What a rule write came to.
#[derive(Clone, Debug, PartialEq)]
pub enum RuleWrite {
    Saved(ForwardingRule),
    /// The workspace already has [`MAX_ACTIVE_RULES`] active rules.
    TooManyRules,
    NotFound,
}

/// Serializes the writes that count a workspace's active rules.
async fn lock_rules(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<(), StoreError> {
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('forwarding_rules/' || $1::text, 0))",
    )
    .bind(workspace.as_uuid())
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

async fn active_count(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<i64, StoreError> {
    sqlx::query_scalar("SELECT count(*) FROM forwarding_rules WHERE workspace_id = $1 AND active")
        .bind(workspace.as_uuid())
        .fetch_one(connection)
        .await
        .map_err(StoreError::query)
}

/// Creates a rule. A name already used in the workspace (ignoring case) is a conflict
/// ([`StoreError::is_conflict`]). Run it in a transaction: the count of active rules is
/// checked under a lock the transaction holds.
pub async fn create_rule(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    new: &NewRule<'_>,
) -> Result<RuleWrite, StoreError> {
    lock_rules(connection, workspace).await?;
    if new.active && active_count(connection, workspace).await? >= MAX_ACTIVE_RULES {
        return Ok(RuleWrite::TooManyRules);
    }
    let rule = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO forwarding_rules (id, workspace_id, destination_id, name, event_name, filter, \
            mapping, active, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING {COLUMNS}"
    )))
    .bind(new.id)
    .bind(workspace.as_uuid())
    .bind(new.destination_id)
    .bind(new.name)
    .bind(new.event_name)
    .bind(new.filter)
    .bind(new.mapping)
    .bind(new.active)
    .bind(new.created_by.map(|user| user.as_uuid()))
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(RuleWrite::Saved(rule))
}

/// Changes a rule. Turning a rule on counts against the cap like creating an active one.
pub async fn update_rule(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    update: &RuleUpdate<'_>,
) -> Result<RuleWrite, StoreError> {
    lock_rules(connection, workspace).await?;
    let Some(current) = find_rule(connection, workspace, id).await? else {
        return Ok(RuleWrite::NotFound);
    };
    if update.active == Some(true)
        && !current.active
        && active_count(connection, workspace).await? >= MAX_ACTIVE_RULES
    {
        return Ok(RuleWrite::TooManyRules);
    }
    let rule = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE forwarding_rules SET name = COALESCE($3, name), \
            event_name = COALESCE($4, event_name), filter = COALESCE($5, filter), \
            mapping = COALESCE($6, mapping), active = COALESCE($7, active), updated_at = now() \
         WHERE workspace_id = $1 AND id = $2 RETURNING {COLUMNS}"
    )))
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(update.name)
    .bind(update.event_name)
    .bind(update.filter)
    .bind(update.mapping)
    .bind(update.active)
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(RuleWrite::Saved(rule))
}

/// Locks a rule for a change and returns it as it is now: the workspace's rule-write lock, then
/// the row itself. A caller that validates a merged change reads the rule here, in the
/// transaction that writes it, so no other change lands between the check and the write. `None`
/// when there is no such rule. Run it in a transaction.
pub async fn lock_rule(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<ForwardingRule>, StoreError> {
    lock_rules(connection, workspace).await?;
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM forwarding_rules WHERE workspace_id = $1 AND id = $2 FOR UPDATE"
    )))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)
}

/// Deletes a rule. Its outbox rows stay and are delivered. `None` when there is no such rule.
pub async fn delete_rule(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<ForwardingRule>, StoreError> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "DELETE FROM forwarding_rules WHERE workspace_id = $1 AND id = $2 RETURNING {COLUMNS}"
    )))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)
}

pub async fn find_rule(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<ForwardingRule>, StoreError> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM forwarding_rules WHERE workspace_id = $1 AND id = $2"
    )))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)
}

/// The workspace's rules, ordered by name.
pub async fn list_rules(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<Vec<ForwardingRule>, StoreError> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM forwarding_rules WHERE workspace_id = $1 ORDER BY lower(name), id"
    )))
    .bind(workspace.as_uuid())
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)
}

/// The active rules whose destination is live, which the outbox insert runs.
pub(super) async fn active_rules(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<Vec<ForwardingRule>, StoreError> {
    sqlx::query_as(
        "SELECT r.id, r.destination_id, r.name, r.event_name, r.filter, r.mapping, r.active, \
                r.created_by, r.created_at, r.updated_at \
         FROM forwarding_rules r \
         JOIN destinations d ON d.workspace_id = r.workspace_id AND d.id = r.destination_id \
         WHERE r.workspace_id = $1 AND r.active AND d.archived_at IS NULL ORDER BY r.id",
    )
    .bind(workspace.as_uuid())
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;
    use crate::{NewDestination, create_destination, create_workspace};

    /// A migrated workspace with one `fake` destination.
    pub(crate) async fn fixture(pool: &PgPool, slug: &str) -> (WorkspaceId, Uuid) {
        crate::migrate(pool).await.unwrap();
        let ws = create_workspace(
            pool,
            &WorkspaceName::parse(slug).unwrap(),
            &Slug::parse(slug).unwrap(),
        )
        .await
        .unwrap()
        .id;
        (ws, destination_in(pool, ws).await)
    }

    /// A `fake` destination in the workspace.
    pub(crate) async fn destination_in(pool: &PgPool, ws: WorkspaceId) -> Uuid {
        create_destination(
            pool,
            ws,
            &NewDestination {
                id: Uuid::now_v7(),
                kind: "fake",
                name: "sink",
                config: &json!({"mode": "ok"}),
                credential_id: None,
                rate: 10,
                burst: 10,
                created_by: None,
            },
        )
        .await
        .unwrap()
        .id
    }

    /// Adds an active rule on the destination and returns its id.
    pub(crate) async fn add_rule(
        pool: &PgPool,
        ws: WorkspaceId,
        destination: Uuid,
        event_name: &str,
        filter: Value,
        mapping: Value,
    ) -> Uuid {
        let mut connection = pool.acquire().await.unwrap();
        let written = create_rule(
            &mut connection,
            ws,
            &NewRule {
                id: Uuid::now_v7(),
                destination_id: destination,
                name: &format!("rule {}", Uuid::now_v7()),
                event_name,
                filter: &filter,
                mapping: &mapping,
                active: true,
                created_by: None,
            },
        )
        .await
        .unwrap();
        let RuleWrite::Saved(rule) = written else {
            panic!("the rule was not saved");
        };
        rule.id
    }

    pub(crate) async fn payloads(pool: &PgPool, ws: WorkspaceId) -> Vec<Value> {
        sqlx::query_scalar(
            "SELECT payload FROM forwarding_outbox WHERE workspace_id = $1 \
             ORDER BY created_at, id",
        )
        .bind(ws.as_uuid())
        .fetch_all(pool)
        .await
        .unwrap()
    }

    /// Outbox rows of the workspace.
    pub(crate) async fn outbox_rows(pool: &PgPool, ws: WorkspaceId) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM forwarding_outbox WHERE workspace_id = $1")
            .bind(ws.as_uuid())
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// `event.forward` jobs, in any state.
    pub(crate) async fn forward_jobs(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'event.forward'")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    fn new_rule<'a>(
        destination: Uuid,
        name: &'a str,
        filter: &'a Value,
        mapping: &'a Value,
    ) -> NewRule<'a> {
        NewRule {
            id: Uuid::now_v7(),
            destination_id: destination,
            name,
            event_name: "order",
            filter,
            mapping,
            active: true,
            created_by: None,
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn at_most_twenty_rules_are_active_and_a_name_is_unique(pool: PgPool) {
        let (ws, destination) = fixture(&pool, "rules").await;
        let (filter, mapping) = (json!([]), json!({"id": {"from": "event.id"}}));
        let mut tx = pool.acquire().await.unwrap();
        for n in 0..MAX_ACTIVE_RULES {
            let name = format!("rule {n}");
            let written = create_rule(
                &mut tx,
                ws,
                &new_rule(destination, &name, &filter, &mapping),
            )
            .await
            .unwrap();
            assert!(matches!(written, RuleWrite::Saved(_)), "{n}");
        }

        let extra = create_rule(
            &mut tx,
            ws,
            &new_rule(destination, "one more", &filter, &mapping),
        )
        .await
        .unwrap();
        assert_eq!(extra, RuleWrite::TooManyRules);
        let mut inactive = new_rule(destination, "dormant", &filter, &mapping);
        inactive.active = false;
        let RuleWrite::Saved(dormant) = create_rule(&mut tx, ws, &inactive).await.unwrap() else {
            panic!("an inactive rule is not counted");
        };
        let turned_on = update_rule(
            &mut tx,
            ws,
            dormant.id,
            &RuleUpdate {
                active: Some(true),
                ..RuleUpdate::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(turned_on, RuleWrite::TooManyRules);
        let mut same_name = new_rule(destination, "RULE 0", &filter, &mapping);
        same_name.active = false;
        let clash = create_rule(&mut tx, ws, &same_name).await.unwrap_err();
        assert!(clash.is_conflict());
        assert_eq!(list_rules(&mut tx, ws).await.unwrap().len(), 21);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_rule_is_changed_found_and_deleted(pool: PgPool) {
        let (ws, destination) = fixture(&pool, "crud").await;
        let (filter, mapping) = (json!([]), json!({"id": {"from": "event.id"}}));
        let mut connection = pool.acquire().await.unwrap();
        let RuleWrite::Saved(rule) = create_rule(
            &mut connection,
            ws,
            &new_rule(destination, "a", &filter, &mapping),
        )
        .await
        .unwrap() else {
            panic!("saved");
        };

        let RuleWrite::Saved(changed) = update_rule(
            &mut connection,
            ws,
            rule.id,
            &RuleUpdate {
                name: Some("b"),
                active: Some(false),
                ..RuleUpdate::default()
            },
        )
        .await
        .unwrap() else {
            panic!("saved");
        };
        assert_eq!((changed.name.as_str(), changed.active), ("b", false));
        assert_eq!(changed.event_name, "order");
        assert_eq!(
            update_rule(&mut connection, ws, Uuid::now_v7(), &RuleUpdate::default())
                .await
                .unwrap(),
            RuleWrite::NotFound
        );
        assert!(active_rules(&mut connection, ws).await.unwrap().is_empty());
        assert!(
            delete_rule(&mut connection, ws, rule.id)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            find_rule(&mut connection, ws, rule.id)
                .await
                .unwrap()
                .is_none()
        );
    }
}
