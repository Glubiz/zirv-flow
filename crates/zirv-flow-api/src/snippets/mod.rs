//! Versioned snippets (architecture 11.3 and 11.7): drafts, preview, publish into immutable
//! versions, and where-used.
//!
//! Permissions: `Read` reads, `Write` saves drafts and previews, `Admin` publishes and archives. A
//! caller with no access gets 404.
//!
//! The store keeps the content as JSON; the content is parsed as `SnippetContent` when a draft is
//! saved and fully validated, through the render pool, when it is published. Audit rows hold ids,
//! names and counts, never content.

pub(crate) mod crud;
pub(crate) mod preview;
pub(crate) mod publish;
pub(crate) mod where_used;

use serde_json::Value;

use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_store::{
    Asset, PgConnection, SnippetError, SnippetTemplate, SnippetThemeUse, StoreError,
};
use zirv_flow_templates::{Block, BlocksError, ImageSrc, Issue, SnippetContent, code, severity_of};

use crate::problem::{ApiError, SegmentRefProblem};

/// The problem for a refused snippet write.
pub(crate) fn snippet_error(error: SnippetError) -> ApiError {
    match error {
        SnippetError::NotFound => ApiError::not_found(),
        SnippetError::Archived => ApiError::snippet_archived(),
        SnippetError::RevisionMismatch => ApiError::snippet_revision_mismatch(),
        SnippetError::InUse { templates, themes } => {
            ApiError::snippet_in_use(template_refs(&templates), theme_refs(&themes))
        }
        SnippetError::Store(error) => name_taken(error),
    }
}

/// The templates as a problem body lists them.
fn template_refs(templates: &[SnippetTemplate]) -> Vec<SegmentRefProblem> {
    templates
        .iter()
        .map(|template| SegmentRefProblem {
            id: template.id,
            name: template.name.clone(),
        })
        .collect()
}

/// The themes as a problem body lists them.
fn theme_refs(themes: &[SnippetThemeUse]) -> Vec<SegmentRefProblem> {
    themes
        .iter()
        .map(|theme| SegmentRefProblem {
            id: theme.id,
            name: theme.name.clone(),
        })
        .collect()
}

/// A unique violation on a snippet write is a name that is taken.
pub(crate) fn name_taken(error: StoreError) -> ApiError {
    if error.is_conflict() {
        ApiError::new(
            axum::http::StatusCode::CONFLICT,
            "a snippet with that name already exists",
        )
    } else {
        error.into()
    }
}

/// The content as the validator reads it, or the 422 that says it is not a block list. A draft
/// may be incomplete; it cannot be a value the block schema does not parse, so the editor, the
/// preview and the publish read the same thing.
pub(crate) fn parse_content(value: &Value) -> Result<SnippetContent, ApiError> {
    serde_json::from_value(value.clone()).map_err(|error| {
        let text = error.to_string();
        // serde lists the block types when the `type` of a block is not one of them; the
        // templates crate only classifies a block document's sections, not a snippet's blocks.
        if text.contains("unknown variant") && text.contains("`heading`") {
            ApiError::template_unknown_block()
        } else if matches!(BlocksError::from_serde(&error), BlocksError::UnknownField) {
            ApiError::template_unknown_field()
        } else {
            ApiError::snippet_malformed("the content is not a snippet of a known shape")
        }
    })
}

pub(crate) fn issue(code: &'static str, at: String, message: &str) -> Issue {
    Issue {
        code,
        severity: severity_of(code),
        at,
        field: None,
        message: Some(message.to_owned()),
    }
}

/// What the references and the shape of a snippet's blocks come to.
pub(crate) struct Checked {
    /// The issues found: an empty snippet, a theme image, an asset that is not live.
    pub issues: Vec<Issue>,
    /// The live assets the blocks name, with the id as the block spells it.
    pub assets: Vec<(String, Asset)>,
}

/// The checks only the API can make, because they read the workspace or know the caller: a snippet
/// holds at least one block, and each `{"asset"}` image names a live asset of the workspace. A
/// theme logo image is not refused: the template or theme that holds the snippet owns that check.
pub(crate) async fn check_references(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    content: &SnippetContent,
) -> Result<Checked, ApiError> {
    let mut checked = Checked {
        issues: Vec::new(),
        assets: Vec::new(),
    };
    if content.blocks.is_empty() {
        checked.issues.push(issue(
            code::MISSING_CONTENT,
            "/blocks".to_owned(),
            "a snippet holds at least one block",
        ));
    }
    for block in &content.blocks {
        let Block::Image(image) = block else {
            continue;
        };
        let at = format!("/blocks/block/{}/src", image.id);
        match &image.src {
            ImageSrc::Url(_) => {}
            ImageSrc::Theme(_) => {}
            ImageSrc::Asset(named) => {
                let live = match named.parse::<Uuid>() {
                    Ok(id) => zirv_flow_store::find_asset(&mut *connection, workspace, id).await?,
                    Err(_) => None,
                };
                match live {
                    Some(asset) => checked.assets.push((named.clone(), asset)),
                    None => checked.issues.push(issue(
                        code::BROKEN_ASSET_REFERENCE,
                        at,
                        "the image names an asset that does not exist or was deleted",
                    )),
                }
            }
        }
    }
    Ok(checked)
}

#[cfg(test)]
mod acceptance;

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zirv_flow_templates::Severity;

    use super::*;

    #[test]
    fn content_must_be_a_block_list_of_known_blocks() {
        assert!(parse_content(&json!({"blocks": []})).is_ok());
        assert!(parse_content(&json!({})).is_ok());
        let code = |value: Value| parse_content(&value).unwrap_err().code();
        assert_eq!(code(json!("text")), Some("snippet_malformed"));
        assert_eq!(
            code(json!({"blocks": [{"type": "carousel", "id": "c"}]})),
            Some("template_unknown_block")
        );
        assert_eq!(
            code(json!({"blocks": [], "theme": "none"})),
            Some("template_unknown_field")
        );
    }

    #[test]
    fn a_section_still_parses_so_the_publish_can_name_it() {
        let content = parse_content(&json!({"blocks": [], "sections": [
            {"id": "s", "columns": [[]]}]}))
        .unwrap();
        assert_eq!(content.sections.len(), 1);
    }

    #[test]
    fn the_archive_refusal_lists_the_templates() {
        let id = Uuid::nil();
        let error = snippet_error(SnippetError::InUse {
            themes: vec![SnippetThemeUse {
                id,
                name: "Brand".into(),
            }],
            templates: vec![SnippetTemplate {
                id,
                name: "Welcome".into(),
                in_draft: true,
                in_current: false,
            }],
        });
        assert_eq!(error.code(), Some("snippet_in_use"));
        assert_eq!(error.status_code(), axum::http::StatusCode::CONFLICT);
        assert_eq!(
            snippet_error(SnippetError::RevisionMismatch).code(),
            Some("snippet_revision_mismatch")
        );
        assert_eq!(
            snippet_error(SnippetError::Archived).code(),
            Some("snippet_archived")
        );
    }

    #[test]
    fn an_issue_takes_its_severity_from_its_code() {
        let blocking = issue(code::MISSING_CONTENT, "/blocks".into(), "m");
        let warning = issue(code::IMAGE_WITHOUT_ALT, "/blocks".into(), "m");
        assert_eq!(blocking.severity, Severity::Blocking);
        assert_eq!(warning.severity, Severity::Warning);
    }
}
