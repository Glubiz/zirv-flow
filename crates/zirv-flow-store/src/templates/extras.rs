//! The inputs a block email renders with beyond its context (architecture 11.4): the theme in
//! effect, the current published version of each snippet it names, and the URL of each asset.
//! The documents, tokens and contents are opaque JSON here; the templates crate turns what this
//! loads into `RenderExtras`, so a preview, a publish check, a send and a test send resolve the
//! same way.
//!
//! An archived snippet or theme still resolves by its current version: archiving hides it from
//! pickers and never breaks a send. One with no published version resolves to nothing.

use std::collections::BTreeMap;

use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::StoreError;
use crate::assets::asset_url;
use crate::themes::{EffectiveTheme, ThemeChoice, effective_theme};

/// Which theme a document asks for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ThemeRequest {
    None,
    Default,
    /// A theme id as the document spells it.
    Id(String),
}

/// What a document names: its theme choice and the ids of its snippet blocks and images, as
/// spelled.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtrasRequest {
    pub theme: ThemeRequest,
    /// Whether the theme footer closes the email; only then is it loaded and recorded.
    pub footer: bool,
    pub snippets: Vec<String>,
    pub assets: Vec<String>,
}

/// The current published version of a snippet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedSnippet {
    pub version: i32,
    pub content: Value,
}

/// What was found. Anything absent was not found: unknown, unpublished or deleted.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LoadedInputs {
    /// The theme in effect, if the document asks for one and it resolves.
    pub theme: Option<EffectiveTheme>,
    /// The snippets of the document and the theme footer, by id as spelled.
    pub snippets: BTreeMap<String, LoadedSnippet>,
    /// The URL of each live asset the document, its snippets and the theme logo name, by id as
    /// spelled.
    pub assets: BTreeMap<String, String>,
}

/// Loads what `request` names. `public_url` is what asset URLs are built on (`server.public_url`
/// without a trailing slash); with none they are the relative `/a/...` path.
pub async fn load_render_inputs(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    request: &ExtrasRequest,
    public_url: Option<&str>,
) -> Result<LoadedInputs, StoreError> {
    let choice = match &request.theme {
        ThemeRequest::None => ThemeChoice::None,
        ThemeRequest::Default => ThemeChoice::Default,
        ThemeRequest::Id(id) => ThemeChoice::Named(id.parse().unwrap_or(Uuid::nil())),
    };
    let theme = effective_theme(connection, workspace, choice).await?;

    let mut wanted = request.snippets.clone();
    let mut assets = request.assets.clone();
    if let Some(theme) = &theme {
        if request.footer
            && let Some(footer) = text_at(&theme.tokens, "footer_snippet")
            && !wanted.iter().any(|id| id == footer)
        {
            wanted.push(footer.to_owned());
        }
        assets.extend(text_at(&theme.tokens, "logo").map(str::to_owned));
    }
    let snippets = current_snippets(connection, workspace, &wanted).await?;
    for snippet in snippets.values() {
        assets.extend(image_assets(&snippet.content));
    }
    let assets = asset_urls(connection, workspace, &assets, public_url).await?;
    Ok(LoadedInputs {
        theme,
        snippets,
        assets,
    })
}

fn text_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// The ids of the assets the images of a snippet's content name.
fn image_assets(content: &Value) -> impl Iterator<Item = String> + '_ {
    content
        .get("blocks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|block| block.get("src")?.get("asset")?.as_str())
        .map(str::to_owned)
}

async fn current_snippets(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    wanted: &[String],
) -> Result<BTreeMap<String, LoadedSnippet>, StoreError> {
    let ids: Vec<Uuid> = wanted.iter().filter_map(|id| id.parse().ok()).collect();
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let rows: Vec<(Uuid, i32, Value)> = sqlx::query_as(
        "SELECT s.id, v.version, v.content FROM snippets s \
         JOIN snippet_versions v ON v.snippet_id = s.id AND v.version = s.current_version \
         WHERE s.workspace_id = $1 AND s.id = ANY($2)",
    )
    .bind(workspace.as_uuid())
    .bind(&ids)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    // Keyed as the caller spelled each id, because that is how a document looks it up.
    Ok(wanted
        .iter()
        .filter_map(|spelled| {
            let id: Uuid = spelled.parse().ok()?;
            let (_, version, content) = rows.iter().find(|(found, ..)| *found == id)?;
            Some((
                spelled.clone(),
                LoadedSnippet {
                    version: *version,
                    content: content.clone(),
                },
            ))
        })
        .collect())
}

async fn asset_urls(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    named: &[String],
    public_url: Option<&str>,
) -> Result<BTreeMap<String, String>, StoreError> {
    let ids: Vec<Uuid> = named.iter().filter_map(|id| id.parse().ok()).collect();
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let rows: Vec<(Uuid, Vec<u8>, String)> = sqlx::query_as(
        "SELECT id, sha256, ext FROM assets \
         WHERE workspace_id = $1 AND id = ANY($2) AND deleted_at IS NULL",
    )
    .bind(workspace.as_uuid())
    .bind(&ids)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(named
        .iter()
        .filter_map(|spelled| {
            let id: Uuid = spelled.parse().ok()?;
            let (_, sha256, ext) = rows.iter().find(|(found, ..)| *found == id)?;
            Some((spelled.clone(), asset_url(public_url, sha256, ext)))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;
    use crate::assets::tests_support::upload;
    use crate::snippets::{archive_snippet, lock_snippet, publish_snippet};
    use crate::templates::tests_support::{trace, workspace};
    use crate::themes::archive_theme;
    use crate::{begin, commit, set_default_theme};

    async fn publish_snippet_with(pool: &PgPool, ws: WorkspaceId, id: Uuid) -> i32 {
        let mut tx = begin(pool).await.unwrap();
        let locked = lock_snippet(&mut tx, ws, id).await.unwrap();
        let (_, version) = publish_snippet(&mut tx, ws, &locked, &json!([]), None, &trace())
            .await
            .unwrap();
        commit(tx).await.unwrap();
        version.version
    }

    fn request(theme: ThemeRequest, snippets: &[Uuid], assets: &[Uuid]) -> ExtrasRequest {
        ExtrasRequest {
            theme,
            footer: true,
            snippets: snippets.iter().map(Uuid::to_string).collect(),
            assets: assets.iter().map(Uuid::to_string).collect(),
        }
    }

    async fn load(
        pool: &PgPool,
        ws: WorkspaceId,
        request: &ExtrasRequest,
        public_url: Option<&str>,
    ) -> LoadedInputs {
        let mut connection = pool.acquire().await.unwrap();
        load_render_inputs(&mut connection, ws, request, public_url)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_snippet_loads_at_its_current_version_even_when_archived_and_not_when_unpublished(
        pool: PgPool,
    ) {
        use crate::snippets::tests_support::create;
        let ws = workspace(&pool, "one").await;
        let live = create(&pool, ws, "Live").await;
        let old = create(&pool, ws, "Old").await;
        let draft = create(&pool, ws, "Draft").await;
        publish_snippet_with(&pool, ws, live.id).await;
        publish_snippet_with(&pool, ws, live.id).await;
        publish_snippet_with(&pool, ws, old.id).await;
        let mut connection = pool.acquire().await.unwrap();
        archive_snippet(&mut connection, ws, old.id).await.unwrap();
        drop(connection);

        let loaded = load(
            &pool,
            ws,
            &request(
                ThemeRequest::None,
                &[live.id, old.id, draft.id, Uuid::now_v7()],
                &[],
            ),
            None,
        )
        .await;

        let versions: BTreeMap<String, i32> = loaded
            .snippets
            .iter()
            .map(|(id, snippet)| (id.clone(), snippet.version))
            .collect();
        assert_eq!(
            versions,
            BTreeMap::from([(live.id.to_string(), 2), (old.id.to_string(), 1)])
        );
        assert!(loaded.theme.is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_theme_choice_resolves_the_default_a_named_archived_theme_and_nothing_for_none(
        pool: PgPool,
    ) {
        use crate::themes::tests_support::{create, publish};
        let ws = workspace(&pool, "one").await;
        let default = create(&pool, ws, "Default").await;
        let named = create(&pool, ws, "Named").await;
        let draft = create(&pool, ws, "Draft").await;
        publish(&pool, ws, default.id).await;
        publish(&pool, ws, named.id).await;
        let mut connection = pool.acquire().await.unwrap();
        set_default_theme(&mut connection, ws, default.id)
            .await
            .unwrap();
        archive_theme(&mut connection, ws, named.id).await.unwrap();
        drop(connection);
        let theme_of = |theme| {
            let pool = &pool;
            async move {
                load(pool, ws, &request(theme, &[], &[]), None)
                    .await
                    .theme
                    .map(|theme| theme.theme_id)
            }
        };

        assert_eq!(theme_of(ThemeRequest::Default).await, Some(default.id));
        assert_eq!(
            theme_of(ThemeRequest::Id(named.id.to_string())).await,
            Some(named.id)
        );
        assert_eq!(theme_of(ThemeRequest::Id(draft.id.to_string())).await, None);
        assert_eq!(theme_of(ThemeRequest::Id("not-an-id".into())).await, None);
        assert_eq!(theme_of(ThemeRequest::None).await, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_theme_footer_logo_and_snippet_assets_load_with_urls_built_on_the_public_url(
        pool: PgPool,
    ) {
        use crate::snippets::{NewSnippet, create_snippet, save_snippet_draft};
        use crate::themes::{NewTheme, create_theme};
        let ws = workspace(&pool, "one").await;
        let logo = upload(&pool, ws, "logo", b"logo-bytes").await.asset;
        let inner = upload(&pool, ws, "inner", b"inner-bytes").await.asset;
        let gone = Uuid::now_v7();
        let mut connection = pool.acquire().await.unwrap();
        let footer = Uuid::now_v7();
        create_snippet(
            &mut connection,
            ws,
            &NewSnippet {
                id: footer,
                name: "Footer".into(),
                content: json!({"blocks": []}),
            },
            None,
        )
        .await
        .unwrap();
        save_snippet_draft(
            &mut connection,
            ws,
            footer,
            &json!({"blocks": [
                {"type": "image", "id": "i", "alt": "x", "src": {"asset": inner.id.to_string()}}
            ]}),
            None,
            None,
        )
        .await
        .unwrap();
        let theme = Uuid::now_v7();
        create_theme(
            &mut connection,
            ws,
            &NewTheme {
                id: theme,
                name: "Brand".into(),
                tokens: json!({
                    "primary": "#112233", "text": "#222222", "background": "#ffffff",
                    "logo": logo.id.to_string(), "footer_snippet": footer.to_string()
                }),
            },
            None,
        )
        .await
        .unwrap();
        drop(connection);
        publish_snippet_with(&pool, ws, footer).await;
        crate::themes::tests_support::publish(&pool, ws, theme).await;

        let loaded = load(
            &pool,
            ws,
            &request(ThemeRequest::Id(theme.to_string()), &[], &[gone]),
            Some("https://flow.example.com"),
        )
        .await;

        assert!(loaded.snippets.contains_key(&footer.to_string()));
        assert_eq!(loaded.assets.len(), 2);
        assert!(!loaded.assets.contains_key(&gone.to_string()));
        for asset in [&logo, &inner] {
            let url = &loaded.assets[&asset.id.to_string()];
            assert!(url.starts_with("https://flow.example.com/a/"), "{url}");
            assert!(url.ends_with(".png"), "{url}");
        }
        let mut without_footer = request(ThemeRequest::Id(theme.to_string()), &[], &[]);
        without_footer.footer = false;
        let loaded = load(&pool, ws, &without_footer, None).await;
        assert!(
            loaded.snippets.is_empty(),
            "a footer that is not rendered is not loaded"
        );
        assert!(loaded.theme.is_some());
        let relative = load(
            &pool,
            ws,
            &request(ThemeRequest::None, &[], &[logo.id]),
            None,
        )
        .await;
        assert!(relative.assets[&logo.id.to_string()].starts_with("/a/"));
    }
}
