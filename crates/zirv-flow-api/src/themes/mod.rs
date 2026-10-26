//! Versioned themes and the workspace default theme (architecture 11.3 and 11.7): drafts, preview,
//! publish into immutable versions, the default, and where-used.
//!
//! Permissions: `Read` reads, `Write` saves drafts and previews, `Admin` publishes, sets the
//! default and archives. A caller with no access gets 404.
//!
//! The store keeps the tokens as JSON; they are parsed as `ThemeTokens` when a draft is saved and
//! fully validated, through the render pool, when the theme is published. Audit rows hold ids and
//! names, never token values.

pub(crate) mod crud;
pub(crate) mod default;
pub(crate) mod preview;
pub(crate) mod publish;
pub(crate) mod where_used;

use serde_json::Value;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_store::{Asset, PgConnection, StoreError, ThemeError};
use zirv_flow_templates::{Issue, ThemeTokens, code, severity_of};

use crate::problem::ApiError;

/// The problem for a refused theme write.
pub(crate) fn theme_error(error: ThemeError) -> ApiError {
    match error {
        ThemeError::NotFound => ApiError::not_found(),
        ThemeError::Archived => ApiError::theme_archived(),
        ThemeError::RevisionMismatch => ApiError::theme_revision_mismatch(),
        ThemeError::IsDefault => ApiError::theme_is_default(),
        ThemeError::NotPublished => ApiError::theme_not_publishable_default(
            "the theme has no published version, so it cannot be the default",
        ),
        ThemeError::Store(error) => name_taken(error),
    }
}

/// A unique violation on a theme write is a name that is taken.
pub(crate) fn name_taken(error: StoreError) -> ApiError {
    if error.is_conflict() {
        ApiError::new(
            axum::http::StatusCode::CONFLICT,
            "a theme with that name already exists",
        )
    } else {
        error.into()
    }
}

/// The tokens as the validator reads them, or the 422 that says they are not a set of tokens. A
/// draft may hold tokens a publish refuses; it cannot hold a value that does not parse, so the
/// editor, the preview and the publish read the same thing.
pub(crate) fn parse_tokens(value: &Value) -> Result<ThemeTokens, ApiError> {
    serde_json::from_value(value.clone())
        .map_err(|_| ApiError::theme_malformed("the tokens are not a theme of a known shape"))
}

/// The tokens as the store keeps them: every token present.
pub(crate) fn stored_tokens(tokens: &ThemeTokens) -> Result<Value, ApiError> {
    serde_json::to_value(tokens).map_err(|_| ApiError::internal())
}

fn issue(code: &'static str, at: &str, message: &str) -> Issue {
    Issue {
        code,
        severity: severity_of(code),
        at: at.to_owned(),
        field: None,
        message: Some(message.to_owned()),
    }
}

/// What the references of a theme come to.
pub(crate) struct Checked {
    /// The issues found: a logo that is not a live asset, a footer that cannot be used.
    pub issues: Vec<Issue>,
    /// The live logo asset, if the theme names one that is.
    pub logo: Option<Asset>,
    /// The footer snippet, if the theme names one that is live and published.
    pub footer: Option<Uuid>,
}

/// The checks only the API can make, because they read the workspace: the logo must be a live
/// asset of the workspace, and the footer a snippet of the workspace that is not archived and has
/// a published version. Inside a transaction it holds the snippet row, so the footer cannot be
/// archived before the transaction ends.
pub(crate) async fn check_references(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    tokens: &ThemeTokens,
) -> Result<Checked, ApiError> {
    let mut checked = Checked {
        issues: Vec::new(),
        logo: None,
        footer: None,
    };
    if let Some(named) = &tokens.logo {
        let live = match named.parse::<Uuid>() {
            Ok(id) => zirv_flow_store::find_asset(&mut *connection, workspace, id).await?,
            Err(_) => None,
        };
        match live {
            Some(asset) => checked.logo = Some(asset),
            None => checked.issues.push(issue(
                code::BROKEN_ASSET_REFERENCE,
                "/theme/logo",
                "the logo names an asset that does not exist or was deleted",
            )),
        }
    }
    if let Some(named) = &tokens.footer_snippet {
        let id = named.parse::<Uuid>().ok();
        let usable = match id {
            Some(id) => zirv_flow_store::footer_snippet_usable(connection, workspace, id).await?,
            None => false,
        };
        match id.filter(|_| usable) {
            Some(id) => checked.footer = Some(id),
            None => checked.issues.push(issue(
                code::BROKEN_SNIPPET_REFERENCE,
                "/theme/footer_snippet",
                "the footer is not a snippet that is live and published",
            )),
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
    fn tokens_must_be_a_known_set_and_any_part_may_be_left_out() {
        assert!(parse_tokens(&json!({})).is_ok());
        assert!(parse_tokens(&json!({"primary": "#123456", "logo": null})).is_ok());
        for bad in [
            json!("text"),
            json!({"colour": "#fff"}),
            json!({"primary": 3}),
        ] {
            assert_eq!(
                parse_tokens(&bad).unwrap_err().code(),
                Some("theme_malformed")
            );
        }
    }

    #[test]
    fn stored_tokens_carry_every_token() {
        let tokens = parse_tokens(&json!({"primary": "#123456"})).unwrap();

        let stored = stored_tokens(&tokens).unwrap();

        assert_eq!(stored["primary"], "#123456");
        assert_eq!(stored["font"], "arial");
        assert_eq!(parse_tokens(&stored).unwrap(), tokens);
    }

    #[test]
    fn a_broken_reference_blocks_and_each_refusal_has_its_code() {
        assert_eq!(
            issue(code::BROKEN_SNIPPET_REFERENCE, "/theme/footer_snippet", "m").severity,
            Severity::Blocking
        );
        let status = |error: ThemeError| {
            let problem = theme_error(error);
            (problem.status_code(), problem.code())
        };
        assert_eq!(
            status(ThemeError::IsDefault),
            (axum::http::StatusCode::CONFLICT, Some("theme_is_default"))
        );
        assert_eq!(
            status(ThemeError::NotPublished),
            (
                axum::http::StatusCode::CONFLICT,
                Some("theme_not_publishable_default")
            )
        );
        assert_eq!(
            status(ThemeError::RevisionMismatch).1,
            Some("theme_revision_mismatch")
        );
        assert_eq!(status(ThemeError::Archived).1, Some("theme_archived"));
        assert_eq!(status(ThemeError::NotFound).1, None);
    }
}
