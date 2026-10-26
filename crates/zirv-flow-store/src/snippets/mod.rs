//! Versioned snippets (architecture 11.3 and 11.7): a block list kept outside any template, with
//! one editable draft and immutable published versions, and the reads that say where a snippet is
//! used.
//!
//! The content is opaque JSON here. The API parses and validates it through the templates crate
//! and the render pool, so this module never sees a block and a new block type changes no table.
//!
//! None of these tables is person-scoped: a snippet holds no person, address or trait value, so
//! the erasure scan and the identity merge have nothing to do here.

mod crud;
mod publish;
mod where_used;

pub use crud::{
    NewSnippet, Snippet, SnippetDraft, SnippetError, SnippetPatch, SnippetTemplate,
    archive_snippet, create_snippet, find_snippet, find_snippet_draft, list_snippets,
    save_snippet_draft, update_snippet,
};
pub use publish::{
    LockedSnippet, SnippetVersion, SnippetVersionSummary, find_snippet_version,
    list_snippet_versions, lock_snippet, publish_snippet,
};
pub use where_used::{SnippetThemeUse, SnippetUse, snippet_where_used};

/// Fixtures for the tests of this module.
#[cfg(test)]
pub(crate) mod tests_support {
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use uuid::Uuid;
    use zirv_flow_core::WorkspaceId;

    use super::{NewSnippet, Snippet, create_snippet};

    pub(crate) fn content(marker: &str) -> Value {
        json!({"blocks": [{"type": "heading", "id": "h", "level": 2, "text": marker}]})
    }

    pub(crate) fn new_snippet(name: &str) -> NewSnippet {
        NewSnippet {
            id: Uuid::now_v7(),
            name: name.to_owned(),
            content: content("draft"),
        }
    }

    pub(crate) async fn create(pool: &PgPool, ws: WorkspaceId, name: &str) -> Snippet {
        let mut connection = pool.acquire().await.unwrap();
        create_snippet(&mut connection, ws, &new_snippet(name), None)
            .await
            .unwrap()
            .0
    }
}
