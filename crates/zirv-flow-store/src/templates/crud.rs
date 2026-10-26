//! Creating, reading, changing and archiving templates and saving their drafts. Every function is
//! scoped to one workspace. A write runs in the caller's transaction, together with its audit
//! entry.

use std::fmt;

use serde_json::Value;
use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{UserId, WorkspaceId};

use crate::StoreError;

/// A template: its settings and which version is current. The content is in its draft and its
/// versions.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct Template {
    pub id: Uuid,
    pub name: String,
    /// `email` or `webhook`; the database checks the set.
    pub channel: String,
    /// `marketing` or `transactional`; the database checks the set.
    pub category: String,
    /// `inherit` follows the workspace UTM setting; `off` never tags this template's links. The
    /// database checks the set.
    pub utm: String,
    /// The sender identity the draft's document names, or none. Derived: every draft write sets
    /// it from `email.sender`, and it is none when that names no sender identity of this
    /// workspace. It is never set on its own.
    pub sender_identity_id: Option<Uuid>,
    /// The newest published version; none before the first publish.
    pub current_version: Option<i32>,
    pub archived_at: Option<OffsetDateTime>,
    pub created_by: Option<Uuid>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// The one editable copy of a template's content.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct TemplateDraft {
    pub template_id: Uuid,
    /// The content document. It may be incomplete; only a publish validates it.
    pub document: Value,
    /// How many times the draft has been saved; the first save is 1.
    pub revision: i32,
    pub updated_by: Option<Uuid>,
    pub updated_at: OffsetDateTime,
}

macro_rules! template_columns {
    () => {
        "id, name, channel, category, sender_identity_id, current_version, archived_at, \
         created_by, created_at, updated_at, utm"
    };
}

macro_rules! draft_columns {
    () => {
        "template_id, document, revision, updated_by, updated_at"
    };
}

pub(super) use template_columns;

/// A template to create, with its first draft.
#[derive(Clone, Debug)]
pub struct NewTemplate {
    pub id: Uuid,
    pub name: String,
    pub channel: String,
    pub category: String,
    /// The first draft.
    pub document: Value,
}

/// The settings to change; `None` leaves one as it is. The channel and the category are fixed
/// when the template is made.
#[derive(Clone, Debug, Default)]
pub struct TemplatePatch {
    pub name: Option<String>,
    /// `inherit` or `off`; the caller checks the set (the database does too).
    pub utm: Option<String>,
}

/// The sender identity id a content document names, if it names one: `email.sender` of a code
/// document, `blocks.sender` of a block document (both a UUID). A sender that is not a UUID names
/// no identity.
pub fn document_sender(document: &Value) -> Option<Uuid> {
    let pointer = if document.pointer("/mode").and_then(Value::as_str) == Some("blocks") {
        "/blocks/sender"
    } else {
        "/email/sender"
    };
    document
        .pointer(pointer)
        .and_then(Value::as_str)
        .and_then(|sender| sender.trim().parse().ok())
}

/// Why a template write was refused.
#[derive(Debug)]
pub enum TemplateError {
    /// No such template in the workspace.
    NotFound,
    /// The template is archived and no longer changes.
    Archived,
    /// A name already taken is [`StoreError::is_conflict`].
    Store(StoreError),
}

impl From<StoreError> for TemplateError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl fmt::Display for TemplateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("no such template"),
            Self::Archived => formatter.write_str("the template is archived"),
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for TemplateError {}

/// Creates a template with its first draft. A live name already taken, ignoring case, is a
/// conflict.
pub async fn create_template(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    new: &NewTemplate,
    created_by: Option<UserId>,
) -> Result<(Template, TemplateDraft), StoreError> {
    let template: Template = sqlx::query_as(concat!(
        "INSERT INTO templates (id, workspace_id, name, channel, category, sender_identity_id, \
            created_by) VALUES ($1, $2, $3, $4, $5, \
            (SELECT id FROM sender_identities WHERE workspace_id = $2 AND id = $6), $7) \
         RETURNING ",
        template_columns!()
    ))
    .bind(new.id)
    .bind(workspace.as_uuid())
    .bind(&new.name)
    .bind(&new.channel)
    .bind(&new.category)
    .bind(document_sender(&new.document))
    .bind(created_by.map(|user| user.as_uuid()))
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let draft: TemplateDraft = sqlx::query_as(concat!(
        "INSERT INTO template_drafts (workspace_id, template_id, document, updated_by) \
         VALUES ($1, $2, $3, $4) RETURNING ",
        draft_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(new.id)
    .bind(&new.document)
    .bind(created_by.map(|user| user.as_uuid()))
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)?;
    Ok((template, draft))
}

/// The template, if the workspace has it.
pub async fn find_template<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<Template>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        template_columns!(),
        " FROM templates WHERE workspace_id = $1 AND id = $2"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// The draft of a template of the workspace.
pub async fn find_template_draft<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<TemplateDraft>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        draft_columns!(),
        " FROM template_drafts WHERE workspace_id = $1 AND template_id = $2"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// Reads the template and holds its row until the transaction ends, so two writes to one template
/// run one after the other. Refuses an archived one.
pub(super) async fn lock_live_template(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Template, TemplateError> {
    let held: Template = sqlx::query_as(concat!(
        "SELECT ",
        template_columns!(),
        " FROM templates WHERE workspace_id = $1 AND id = $2 FOR UPDATE"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?
    .ok_or(TemplateError::NotFound)?;
    if held.archived_at.is_some() {
        return Err(TemplateError::Archived);
    }
    Ok(held)
}

/// Share-locks the templates of the workspace among `ids`, in id order, until the transaction ends.
/// A flow publish takes them before it checks that the templates it names are usable: an archive
/// in progress (which holds the row exclusively) makes the publish wait and then see the template
/// archived, and a publish in progress makes an archive wait and then see the flow. An id that
/// names no template is skipped.
pub async fn lock_templates_shared(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    ids: &[Uuid],
) -> Result<(), StoreError> {
    sqlx::query(
        "SELECT id FROM templates WHERE workspace_id = $1 AND id = ANY($2) ORDER BY id FOR SHARE",
    )
    .bind(workspace.as_uuid())
    .bind(ids)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// Changes the given settings of a template that is not archived.
pub async fn update_template(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    patch: &TemplatePatch,
) -> Result<Template, TemplateError> {
    lock_live_template(connection, workspace, id).await?;
    let template = sqlx::query_as(concat!(
        "UPDATE templates SET name = COALESCE($3, name), utm = COALESCE($4, utm), \
            updated_at = now() WHERE workspace_id = $1 AND id = $2 RETURNING ",
        template_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(patch.name.as_deref())
    .bind(patch.utm.as_deref())
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(template)
}

/// Replaces the draft of a template that is not archived and counts the save. The document is not
/// validated: a template is edited in steps, and the publish checks it.
pub async fn save_template_draft(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    document: &Value,
    updated_by: Option<UserId>,
) -> Result<TemplateDraft, TemplateError> {
    lock_live_template(connection, workspace, id).await?;
    let draft = sqlx::query_as(concat!(
        "UPDATE template_drafts SET document = $3, revision = revision + 1, updated_by = $4, \
            updated_at = now(), checks = NULL WHERE workspace_id = $1 AND template_id = $2 RETURNING ",
        draft_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(document)
    .bind(updated_by.map(|user| user.as_uuid()))
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    // The template's sender follows the draft's document.
    sqlx::query(
        "UPDATE templates SET sender_identity_id = \
            (SELECT id FROM sender_identities WHERE workspace_id = $1 AND id = $3) \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(document_sender(document))
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(draft)
}

/// Archives a template. Nothing is deleted: the draft and the versions stay readable.
pub async fn archive_template(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Template, TemplateError> {
    lock_live_template(connection, workspace, id).await?;
    let template = sqlx::query_as(concat!(
        "UPDATE templates SET archived_at = now(), updated_at = now() \
         WHERE workspace_id = $1 AND id = $2 RETURNING ",
        template_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(template)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::templates::tests_support::*;
    use crate::templates::{TemplateListQuery, list_templates};

    fn all() -> TemplateListQuery {
        TemplateListQuery {
            include_archived: true,
            ..TemplateListQuery::default()
        }
    }

    #[test]
    fn a_documents_sender_is_read_where_its_content_type_keeps_it() {
        let id = "0192b1a4-7c3e-7000-8000-00000000a001";
        let code = serde_json::json!({"mode": "code", "email": {"sender": id}});
        let blocks = serde_json::json!({"mode": "blocks", "blocks": {"sender": id}});
        let not_an_identity = serde_json::json!({"mode": "blocks", "blocks": {"sender": "snd_1"}});
        let wrong_place = serde_json::json!({"mode": "blocks", "email": {"sender": id}});

        assert_eq!(document_sender(&code), id.parse().ok());
        assert_eq!(document_sender(&blocks), id.parse().ok());
        assert_eq!(document_sender(&not_an_identity), None);
        assert_eq!(document_sender(&wrong_place), None);
        assert_eq!(document_sender(&serde_json::json!({})), None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_new_template_has_a_draft_and_no_version(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let new = new_template("Welcome");
        let mut connection = pool.acquire().await.unwrap();

        let (template, draft) = create_template(&mut connection, ws, &new, None)
            .await
            .unwrap();

        assert_eq!(template.current_version, None);
        assert_eq!(draft.document, new.document);
        assert_eq!(draft.revision, 1);
        assert_eq!(
            find_template(&pool, ws, new.id).await.unwrap(),
            Some(template)
        );
        assert_eq!(
            find_template_draft(&pool, ws, new.id).await.unwrap(),
            Some(draft)
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_live_name_is_unique_ignoring_case_and_an_archived_name_is_free(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;
        let first = create(&pool, ws, "Welcome").await;
        let mut connection = pool.acquire().await.unwrap();

        let duplicate = create_template(&mut connection, ws, &new_template("WELCOME"), None).await;
        let elsewhere =
            create_template(&mut connection, other, &new_template("Welcome"), None).await;
        archive_template(&mut connection, ws, first.id)
            .await
            .unwrap();
        let reused = create_template(&mut connection, ws, &new_template("Welcome"), None).await;

        assert!(duplicate.unwrap_err().is_conflict());
        assert!(elsewhere.is_ok());
        assert!(reused.is_ok());
        let live = list_templates(&pool, ws, &TemplateListQuery::default())
            .await
            .unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(list_templates(&pool, ws, &all()).await.unwrap().len(), 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn saving_the_draft_counts_and_an_archived_template_refuses_changes(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let template = create(&pool, ws, "Welcome").await;
        let mut connection = pool.acquire().await.unwrap();

        let saved = save_template_draft(&mut connection, ws, template.id, &document("v2"), None)
            .await
            .unwrap();
        archive_template(&mut connection, ws, template.id)
            .await
            .unwrap();
        let after =
            save_template_draft(&mut connection, ws, template.id, &document("v3"), None).await;
        let renamed = update_template(
            &mut connection,
            ws,
            template.id,
            &TemplatePatch {
                name: Some("New".into()),
                ..TemplatePatch::default()
            },
        )
        .await;
        let again = archive_template(&mut connection, ws, template.id).await;

        assert_eq!((saved.revision, saved.document), (2, document("v2")));
        assert!(matches!(after, Err(TemplateError::Archived)));
        assert!(matches!(renamed, Err(TemplateError::Archived)));
        assert!(matches!(again, Err(TemplateError::Archived)));
    }
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_patch_changes_only_the_given_settings(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let template = create(&pool, ws, "Welcome").await;
        let mut connection = pool.acquire().await.unwrap();

        let named = update_template(
            &mut connection,
            ws,
            template.id,
            &TemplatePatch {
                name: Some("Hello".into()),
                ..TemplatePatch::default()
            },
        )
        .await
        .unwrap();
        let untouched =
            update_template(&mut connection, ws, template.id, &TemplatePatch::default())
                .await
                .unwrap();

        assert_eq!(named.name, "Hello");
        assert_eq!(untouched.name, "Hello");
        assert_eq!(named.channel, template.channel);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_utm_override_defaults_to_inherit_and_accepts_only_inherit_or_off(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let template = create(&pool, ws, "Welcome").await;
        let mut connection = pool.acquire().await.unwrap();
        assert_eq!(template.utm, "inherit");

        let off = update_template(
            &mut connection,
            ws,
            template.id,
            &TemplatePatch {
                utm: Some("off".into()),
                ..TemplatePatch::default()
            },
        )
        .await
        .unwrap();
        let kept = update_template(
            &mut connection,
            ws,
            template.id,
            &TemplatePatch {
                name: Some("Hello".into()),
                ..TemplatePatch::default()
            },
        )
        .await
        .unwrap();
        let bad = update_template(
            &mut connection,
            ws,
            template.id,
            &TemplatePatch {
                utm: Some("on".into()),
                ..TemplatePatch::default()
            },
        )
        .await;

        assert_eq!(off.utm, "off");
        assert_eq!(kept.utm, "off");
        assert!(bad.is_err());
    }

    fn document_from(sender: &str) -> Value {
        serde_json::json!({"schema": 1, "channel": "email", "mode": "code",
                           "email": {"subject": "s", "sender": sender, "html": "<p>x</p>"}})
    }

    #[test]
    fn a_document_names_a_sender_only_with_a_uuid() {
        let id = Uuid::now_v7();

        assert_eq!(document_sender(&document_from(&id.to_string())), Some(id));
        assert_eq!(
            document_sender(&document_from(&format!(" {id} "))),
            Some(id)
        );
        assert_eq!(document_sender(&document_from("snd_1")), None);
        assert_eq!(document_sender(&document("x")), None);
        assert_eq!(document_sender(&serde_json::json!({})), None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_templates_sender_follows_the_drafts_document(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let domain = crate::sending::fixtures::plant_domain(&pool, ws, "example.com")
            .await
            .unwrap();
        let sender =
            crate::sending::fixtures::plant_identity(&pool, ws, domain, "news@example.com")
                .await
                .unwrap();
        let mut connection = pool.acquire().await.unwrap();
        let mut new = new_template("Welcome");
        new.document = document_from(&sender.to_string());
        let (created, _) = create_template(&mut connection, ws, &new, None)
            .await
            .unwrap();
        assert_eq!(created.sender_identity_id, Some(sender));

        let sender_of = |pool: PgPool, id: Uuid| async move {
            find_template(&pool, ws, id)
                .await
                .unwrap()
                .unwrap()
                .sender_identity_id
        };
        save_template_draft(&mut connection, ws, created.id, &document("none"), None)
            .await
            .unwrap();
        assert_eq!(sender_of(pool.clone(), created.id).await, None);
        save_template_draft(
            &mut connection,
            ws,
            created.id,
            &document_from(&sender.to_string()),
            None,
        )
        .await
        .unwrap();
        assert_eq!(sender_of(pool.clone(), created.id).await, Some(sender));
        // A sender that is not an identity of the workspace leaves the column empty.
        save_template_draft(
            &mut connection,
            ws,
            created.id,
            &document_from(&Uuid::now_v7().to_string()),
            None,
        )
        .await
        .unwrap();
        assert_eq!(sender_of(pool.clone(), created.id).await, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_template_is_scoped_to_its_workspace(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let template = create(&pool, one, "Welcome").await;
        let mut connection = pool.acquire().await.unwrap();

        assert!(
            find_template(&pool, two, template.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            find_template_draft(&pool, two, template.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(list_templates(&pool, two, &all()).await.unwrap().is_empty());
        assert!(matches!(
            save_template_draft(&mut connection, two, template.id, &document("x"), None).await,
            Err(TemplateError::NotFound)
        ));
        assert!(matches!(
            archive_template(&mut connection, two, template.id).await,
            Err(TemplateError::NotFound)
        ));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn shared_locks_coexist_skip_unknown_ids_and_hold_back_an_archive(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let first = create(&pool, ws, "First").await;
        let second = create(&pool, ws, "Second").await;
        let ids = [second.id, Uuid::now_v7(), first.id];
        let mut one = pool.acquire().await.unwrap();
        let mut two = pool.acquire().await.unwrap();
        let mut sharing = crate::begin(&pool).await.unwrap();
        lock_templates_shared(&mut sharing, ws, &ids).await.unwrap();

        // Another share lock does not wait.
        let mut also = crate::begin(&pool).await.unwrap();
        lock_templates_shared(&mut also, ws, &ids).await.unwrap();
        // An exclusive lock does: it times out while the share locks are held.
        sqlx::query("SET lock_timeout = '200ms'")
            .execute(&mut *one)
            .await
            .unwrap();
        let blocked = archive_template(&mut one, ws, first.id).await;
        assert!(blocked.is_err());
        crate::commit(sharing).await.unwrap();
        crate::commit(also).await.unwrap();
        let archived = archive_template(&mut two, ws, first.id).await;

        assert!(archived.is_ok());
    }
}
