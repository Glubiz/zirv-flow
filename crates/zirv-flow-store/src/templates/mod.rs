//! Message templates (architecture 11.6, 11.7 and 15): the template and its settings, one editable
//! draft, immutable published versions, the merge-field catalog a publish validates against, and
//! the flow versions that name a template.
//!
//! The content document is opaque JSON here. The API validates and renders it through the
//! content-type registry in `zirv-flow-templates`, so this module never sees a content type and a
//! new one changes no table.
//!
//! Only `template_preview_people` is person-scoped: the people an admin saves to preview a template
//! as. The erasure deletes its rows and a merge moves them to the survivor. The other tables hold
//! no person, address or trait value.

mod catalog;
mod crud;
mod extras;
mod library;
mod preview_people;
mod publish;
mod where_used;

pub use catalog::{TemplateFieldCatalog, read_template_field_catalog};
pub use crud::{
    NewTemplate, Template, TemplateDraft, TemplateError, TemplatePatch, archive_template,
    create_template, document_sender, find_template, find_template_draft, save_template_draft,
    update_template,
};
pub use extras::{ExtrasRequest, LoadedInputs, LoadedSnippet, ThemeRequest, load_render_inputs};
pub use library::{
    TemplateListQuery, TemplateSort, TemplateStatus, TemplateSummary, TemplateTheme,
    TemplateThemeFilter, list_templates, set_template_draft_checks, template_active_flow_counts,
};
pub(crate) use preview_people::move_to_survivor;
pub use preview_people::{
    MAX_PREVIEW_PEOPLE, PreviewPerson, PreviewPersonError, add_preview_person, list_preview_people,
    remove_preview_person,
};
pub use publish::{
    LockedTemplate, TemplateVersion, TemplateVersionSummary, find_template_version,
    list_template_versions, lock_template, publish_template,
};
pub use where_used::{
    TemplateFlowUse, TemplateUse, active_flows_using, active_published_flows, template_where_used,
};

/// Fixtures for the tests of this module.
#[cfg(test)]
pub(crate) mod tests_support {
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use uuid::Uuid;
    use zirv_flow_core::{Cause, Slug, TraceContext, WorkspaceId, WorkspaceName};

    use super::{NewTemplate, Template, create_template};
    use crate::create_workspace;

    pub(crate) async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    pub(crate) fn trace() -> TraceContext {
        TraceContext::root(Cause::definition_change(Uuid::now_v7()))
    }

    pub(crate) fn document(marker: &str) -> Value {
        json!({"schema": 1, "channel": "email", "mode": "code",
               "email": {"subject": marker, "html": "<p>x</p>"}})
    }

    pub(crate) fn new_template(name: &str) -> NewTemplate {
        NewTemplate {
            id: Uuid::now_v7(),
            name: name.to_owned(),
            channel: "email".to_owned(),
            category: "marketing".to_owned(),
            document: document("draft"),
        }
    }

    pub(crate) async fn create(pool: &PgPool, ws: WorkspaceId, name: &str) -> Template {
        let mut connection = pool.acquire().await.unwrap();
        create_template(&mut connection, ws, &new_template(name), None)
            .await
            .unwrap()
            .0
    }
}
