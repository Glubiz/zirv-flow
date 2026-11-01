//! Versioned themes and the workspace default theme (architecture 11.3 and 11.7): a set of design
//! tokens kept outside any template, with one editable draft and immutable published versions,
//! the default that block emails follow, the theme an email resolves at render, and the reads that
//! say where a theme is used.
//!
//! The tokens are opaque JSON here. The API parses and validates them through the templates crate
//! and the render pool, so this module never sees a token and a new token changes no table.
//!
//! None of these tables is person-scoped: a theme holds no person, address or trait value, so the
//! erasure scan and the identity merge have nothing to do here.

mod crud;
mod default;
mod publish;
mod where_used;

pub use crud::{
    NewTheme, Theme, ThemeDraft, ThemeError, ThemePatch, archive_theme, create_theme, find_theme,
    find_theme_draft, list_theme_tokens, list_themes, save_theme_draft, update_theme,
};
pub use default::{EffectiveTheme, ThemeChoice, effective_theme, set_default_theme};
pub use publish::{
    LockedTheme, ThemeVersion, ThemeVersionSummary, find_theme_version, footer_snippet_usable,
    list_theme_versions, lock_theme, publish_theme,
};
pub use where_used::{
    ThemeTemplate, ThemeUse, default_live_templates, theme_live_templates, theme_template_counts,
    theme_where_used,
};
pub(crate) use where_used::{default_following_templates, follows_default, is_default_theme};

/// Fixtures for the tests of this module.
#[cfg(test)]
pub(crate) mod tests_support {
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use uuid::Uuid;
    use zirv_flow_core::WorkspaceId;

    use super::{NewTheme, Theme, create_theme, lock_theme, publish_theme};
    use crate::templates::tests_support::trace;
    use crate::{begin, commit};

    /// Tokens that differ by their primary colour.
    pub(crate) fn tokens(primary: &str) -> Value {
        json!({"primary": primary, "text": "#222222", "background": "#ffffff"})
    }

    pub(crate) fn new_theme(name: &str) -> NewTheme {
        NewTheme {
            id: Uuid::now_v7(),
            name: name.to_owned(),
            tokens: tokens("#112233"),
        }
    }

    pub(crate) async fn create(pool: &PgPool, ws: WorkspaceId, name: &str) -> Theme {
        let mut connection = pool.acquire().await.unwrap();
        create_theme(&mut connection, ws, &new_theme(name), None)
            .await
            .unwrap()
            .0
    }

    /// Publishes the theme's draft with no warnings.
    pub(crate) async fn publish(pool: &PgPool, ws: WorkspaceId, id: Uuid) {
        let mut tx = begin(pool).await.unwrap();
        let locked = lock_theme(&mut tx, ws, id).await.unwrap();
        publish_theme(&mut tx, ws, &locked, &json!([]), None, &trace())
            .await
            .unwrap();
        commit(tx).await.unwrap();
    }
}
