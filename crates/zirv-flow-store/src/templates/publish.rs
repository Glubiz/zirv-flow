//! Publishing a template's draft into an immutable version, and reading versions.
//!
//! This crate never sees a content type or the validator: the caller locks the template with
//! [`lock_template`], validates the draft it holds, and passes the same [`LockedTemplate`] to
//! [`publish_template`], so the document that was checked is the document that is published. A
//! caller that finds blocking issues drops its transaction, which writes no version.

use serde_json::Value;
use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{TraceContext, UserId, WorkspaceId};

use super::crud::{
    Template, TemplateDraft, TemplateError, find_template_draft, lock_live_template,
    template_columns,
};
use crate::{NewDecision, StoreError, append_decision};

/// One immutable published version of a template.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct TemplateVersion {
    pub template_id: Uuid,
    pub version: i32,
    /// The document as it was published.
    pub document: Value,
    /// The issues found at publish time: warnings only, because a blocking one refuses the
    /// publish.
    pub validation: Value,
    pub published_at: OffsetDateTime,
    pub published_by: Option<Uuid>,
    /// The trace of the request that published the version.
    pub trace_id: Uuid,
}

macro_rules! version_columns {
    () => {
        "template_id, version, document, validation, published_at, published_by, trace_id"
    };
}

/// A template whose row this transaction holds `FOR UPDATE`, with the draft read under that lock.
/// Only [`lock_template`] makes one, so a publish can only act on content that was read under the
/// lock.
#[derive(Debug)]
pub struct LockedTemplate {
    template: Template,
    draft: TemplateDraft,
}

impl LockedTemplate {
    pub fn template(&self) -> &Template {
        &self.template
    }

    /// The draft the publish would copy.
    pub fn draft(&self) -> &TemplateDraft {
        &self.draft
    }
}

/// Locks a template that is not archived and reads its draft. Writes to the template wait here, so
/// two publishes of one template run one after the other.
pub async fn lock_template(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<LockedTemplate, TemplateError> {
    let template = lock_live_template(connection, workspace, id).await?;
    let draft = find_template_draft(&mut *connection, workspace, id)
        .await?
        .ok_or(TemplateError::NotFound)?;
    Ok(LockedTemplate { template, draft })
}

/// Publishes the locked template's draft as version n + 1 and makes it current, in the caller's
/// transaction, and records a `template.publish` decision under `trace`. `validation` is the
/// issues the caller found, stored with the version.
pub async fn publish_template(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    locked: &LockedTemplate,
    validation: &Value,
    published_by: Option<UserId>,
    trace: &TraceContext,
) -> Result<(Template, TemplateVersion), TemplateError> {
    let template = &locked.template;
    let number = template.current_version.unwrap_or(0) + 1;
    let version: TemplateVersion = sqlx::query_as(concat!(
        "INSERT INTO template_versions (workspace_id, template_id, version, document, validation, \
            published_by, trace_id) VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING ",
        version_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(template.id)
    .bind(number)
    .bind(&locked.draft.document)
    .bind(validation)
    .bind(published_by.map(|user| user.as_uuid()))
    .bind(trace.trace_id.as_uuid())
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let updated: Template = sqlx::query_as(concat!(
        "UPDATE templates SET current_version = $3, updated_at = now() \
         WHERE workspace_id = $1 AND id = $2 RETURNING ",
        template_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(template.id)
    .bind(number)
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    append_decision(
        connection,
        &NewDecision {
            graph: None,
            workspace_id: workspace,
            trace: trace.clone(),
            action: "template.publish",
            outcome: "published",
            reason: None,
            subject_ids: vec![("template", template.id)],
            job: None,
        },
    )
    .await?;
    Ok((updated, version))
}

/// The versions of a template of the workspace, newest first.
pub async fn list_template_versions<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Vec<TemplateVersion>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        version_columns!(),
        " FROM template_versions WHERE workspace_id = $1 AND template_id = $2 \
          ORDER BY version DESC"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// One version of a template of the workspace.
pub async fn find_template_version<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
    version: i32,
) -> Result<Option<TemplateVersion>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        version_columns!(),
        " FROM template_versions WHERE workspace_id = $1 AND template_id = $2 AND version = $3"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(version)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;
    use crate::templates::tests_support::*;
    use crate::templates::{archive_template, save_template_draft};
    use crate::{begin, commit};

    async fn publish(
        pool: &PgPool,
        ws: WorkspaceId,
        id: Uuid,
    ) -> Result<(Template, TemplateVersion), TemplateError> {
        let mut tx = begin(pool).await?;
        let locked = lock_template(&mut tx, ws, id).await?;
        let published = publish_template(&mut tx, ws, &locked, &json!([]), None, &trace()).await?;
        commit(tx).await?;
        Ok(published)
    }

    async fn edit(pool: &PgPool, ws: WorkspaceId, id: Uuid, marker: &str) {
        let mut connection = pool.acquire().await.unwrap();
        save_template_draft(&mut connection, ws, id, &document(marker), None)
            .await
            .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn publish_creates_version_one_then_two_and_keeps_the_draft(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let template = create(&pool, ws, "Welcome").await;

        let (after_one, one) = publish(&pool, ws, template.id).await.unwrap();
        edit(&pool, ws, template.id, "second").await;
        let (after_two, two) = publish(&pool, ws, template.id).await.unwrap();

        assert_eq!((one.version, after_one.current_version), (1, Some(1)));
        assert_eq!((two.version, after_two.current_version), (2, Some(2)));
        assert_eq!(one.document, document("draft"));
        assert_eq!(two.document, document("second"));
        let all = list_template_versions(&pool, ws, template.id)
            .await
            .unwrap();
        assert_eq!(all.iter().map(|v| v.version).collect::<Vec<_>>(), [2, 1]);
        assert_eq!(
            find_template_version(&pool, ws, template.id, 1)
                .await
                .unwrap(),
            Some(one)
        );
        let draft = crate::find_template_draft(&pool, ws, template.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.document, document("second"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_published_version_cannot_be_updated_or_deleted(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let template = create(&pool, ws, "Welcome").await;
        publish(&pool, ws, template.id).await.unwrap();

        let update = sqlx::query("UPDATE template_versions SET document = '{}'::jsonb")
            .execute(&pool)
            .await;
        let delete = sqlx::query("DELETE FROM template_versions WHERE template_id = $1")
            .bind(template.id)
            .execute(&pool)
            .await;

        assert!(
            update
                .unwrap_err()
                .to_string()
                .contains("template versions are immutable")
        );
        assert!(delete.is_err());
        let version = find_template_version(&pool, ws, template.id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(version.document, document("draft"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_dropped_transaction_writes_no_version(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let template = create(&pool, ws, "Welcome").await;

        // What the API does when validation finds a blocking issue: it never publishes.
        let mut tx = begin(&pool).await.unwrap();
        let _locked = lock_template(&mut tx, ws, template.id).await.unwrap();
        drop(tx);
        // And a publish whose transaction is dropped before the commit leaves no trace either.
        let mut tx = begin(&pool).await.unwrap();
        let locked = lock_template(&mut tx, ws, template.id).await.unwrap();
        publish_template(&mut tx, ws, &locked, &json!([]), None, &trace())
            .await
            .unwrap();
        drop(tx);

        assert!(
            list_template_versions(&pool, ws, template.id)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            crate::find_template(&pool, ws, template.id)
                .await
                .unwrap()
                .unwrap()
                .current_version,
            None
        );
        assert_eq!(decisions(&pool, ws).await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn publish_records_a_definition_change_decision_and_the_validation(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let template = create(&pool, ws, "Welcome").await;
        let warnings =
            json!([{"code": "missing_lang", "severity": "warning", "at": "/email/lang"}]);
        let mut tx = begin(&pool).await.unwrap();
        let locked = lock_template(&mut tx, ws, template.id).await.unwrap();

        let (_, version) = publish_template(&mut tx, ws, &locked, &warnings, None, &trace())
            .await
            .unwrap();
        commit(tx).await.unwrap();

        assert_eq!(version.validation, warnings);
        let row: (String, String, String) = sqlx::query_as(
            "SELECT action, outcome, cause_kind FROM decision_records WHERE workspace_id = $1",
        )
        .bind(ws.as_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            row,
            (
                "template.publish".to_owned(),
                "published".to_owned(),
                "definition_change".to_owned()
            )
        );
        assert_eq!(decisions(&pool, ws).await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_archived_or_foreign_template_cannot_be_locked_for_publish(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let template = create(&pool, one, "Welcome").await;
        let mut connection = pool.acquire().await.unwrap();

        let foreign = lock_template(&mut connection, two, template.id).await;
        archive_template(&mut connection, one, template.id)
            .await
            .unwrap();
        let archived = lock_template(&mut connection, one, template.id).await;

        assert!(matches!(foreign, Err(TemplateError::NotFound)));
        assert!(matches!(archived, Err(TemplateError::Archived)));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn versions_are_scoped_to_their_workspace(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let template = create(&pool, one, "Welcome").await;
        publish(&pool, one, template.id).await.unwrap();

        assert!(
            list_template_versions(&pool, two, template.id)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            find_template_version(&pool, two, template.id, 1)
                .await
                .unwrap()
                .is_none()
        );
    }

    async fn decisions(pool: &PgPool, ws: WorkspaceId) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM decision_records WHERE workspace_id = $1 \
             AND action = 'template.publish'",
        )
        .bind(ws.as_uuid())
        .fetch_one(pool)
        .await
        .unwrap()
    }
}
