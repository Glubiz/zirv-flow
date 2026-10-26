//! The wire shape of an isolated render (architecture 11.9): what the render host reads and
//! writes, so this pure crate owns the contract and the host and its pool only move bytes.
//!
//! A request carries the render call and its inputs; a response carries the result or the
//! renderer's typed error. Click tracking is not part of a request: the caller's token function
//! cannot cross a process boundary, so the host renders without tracking and the caller applies
//! [`crate::apply_tracking`] to the result.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::content::blocks::{SnippetContent, validate_snippet};
use crate::content::{ContentError, validate_document_with};
use crate::context::{RenderContext, RenderExtras};
use crate::document::TemplateDocument;
use crate::error::RenderError;
use crate::issue::{FieldCatalog, Issue};
use crate::links::Hooks;
use crate::render::{Rendered, RenderedWebhook, render_email_with, render_webhook};
use crate::theme::{ThemeTokens, validate_theme};

/// One render call.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RenderRequest<'a> {
    Email {
        doc: Cow<'a, TemplateDocument>,
        context: Cow<'a, RenderContext>,
        /// The resolved inputs of a block email; an old request has none.
        #[serde(default, skip_serializing_if = "no_extras")]
        extras: Cow<'a, RenderExtras>,
    },
    Webhook {
        doc: Cow<'a, TemplateDocument>,
        context: Cow<'a, RenderContext>,
    },
    /// The publish checks of `validate`, whose sample render runs in the child like any other.
    Validate {
        doc: Cow<'a, TemplateDocument>,
        catalog: Cow<'a, FieldCatalog>,
        #[serde(default, skip_serializing_if = "no_extras")]
        extras: Cow<'a, RenderExtras>,
    },
    /// The shape and contrast checks of a theme.
    ValidateTheme { theme: Cow<'a, ThemeTokens> },
    /// The checks of a snippet.
    ValidateSnippet {
        snippet: Cow<'a, SnippetContent>,
        catalog: Cow<'a, FieldCatalog>,
    },
}

fn no_extras(extras: &RenderExtras) -> bool {
    *extras == RenderExtras::default()
}

/// The answer to one [`RenderRequest`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RenderResponse {
    Email(Rendered),
    Webhook(RenderedWebhook),
    /// The answer to a validate request.
    Issues {
        issues: Vec<Issue>,
    },
    /// The answer to a validate request for a document no registered content type owns.
    UnsupportedDocument,
    /// The renderer's own error: a fixed code and the field concerned, never template data.
    Error {
        code: String,
        #[serde(default)]
        field: Option<String>,
    },
}

impl RenderRequest<'_> {
    /// Run the call in this process, as the render host does.
    pub fn run(&self) -> RenderResponse {
        match self {
            Self::Email {
                doc,
                context,
                extras,
            } => RenderResponse::from_email(render_email_with(
                doc,
                context,
                &Hooks::untracked(),
                extras,
            )),
            Self::Validate {
                doc,
                catalog,
                extras,
            } => match validate_document_with(doc, catalog, extras) {
                Ok(issues) => RenderResponse::Issues { issues },
                Err(ContentError::UnsupportedDocument | ContentError::Unregistered(_)) => {
                    RenderResponse::UnsupportedDocument
                }
            },
            Self::ValidateTheme { theme } => RenderResponse::Issues {
                issues: validate_theme(theme),
            },
            Self::ValidateSnippet { snippet, catalog } => RenderResponse::Issues {
                issues: validate_snippet(snippet, catalog),
            },
            Self::Webhook { doc, context } => {
                RenderResponse::from_webhook(render_webhook(doc, context))
            }
        }
    }
}

impl RenderResponse {
    /// The response for a rendered email or the renderer's error.
    pub fn from_email(result: Result<Rendered, RenderError>) -> Self {
        match result {
            Ok(rendered) => Self::Email(rendered),
            Err(error) => Self::from_error(&error),
        }
    }

    /// The response for a rendered webhook or the renderer's error.
    pub fn from_webhook(result: Result<RenderedWebhook, RenderError>) -> Self {
        match result {
            Ok(rendered) => Self::Webhook(rendered),
            Err(error) => Self::from_error(&error),
        }
    }

    fn from_error(error: &RenderError) -> Self {
        Self::Error {
            code: error.code().to_owned(),
            field: error.field().map(str::to_owned),
        }
    }

    fn typed_error<T>(code: &str, field: Option<&str>) -> Option<Result<T, RenderError>> {
        RenderError::from_wire(code, field).map(Err)
    }

    /// The email result, or `None` when this response is not an answer to an email request (or
    /// names an error code this crate does not define): a protocol fault.
    pub fn into_email(self) -> Option<Result<Rendered, RenderError>> {
        match self {
            Self::Email(rendered) => Some(Ok(rendered)),
            Self::Error { code, field } => Self::typed_error(&code, field.as_deref()),
            Self::Webhook(_) | Self::Issues { .. } | Self::UnsupportedDocument => None,
        }
    }

    /// The issues of a validate request, or the typed refusal of a document no registered content
    /// type owns; `None` when this response is not the answer to a validate request (an error
    /// response is a render error, which a validate request never gets): a protocol fault.
    pub fn into_issues(self) -> Option<Result<Vec<Issue>, ContentError>> {
        match self {
            Self::Issues { issues } => Some(Ok(issues)),
            Self::UnsupportedDocument => Some(Err(ContentError::UnsupportedDocument)),
            _ => None,
        }
    }

    /// The webhook result, or `None` as for [`into_email`](Self::into_email).
    pub fn into_webhook(self) -> Option<Result<RenderedWebhook, RenderError>> {
        match self {
            Self::Webhook(rendered) => Some(Ok(rendered)),
            Self::Error { code, field } => Self::typed_error(&code, field.as_deref()),
            Self::Email(_) | Self::Issues { .. } | Self::UnsupportedDocument => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{Category, EmailContent};

    fn email_doc(html: &str) -> TemplateDocument {
        TemplateDocument::email_code(EmailContent {
            subject: "Hi".into(),
            sender: None,
            lang: None,
            category: Category::Transactional,
            syntax: Default::default(),
            html: html.into(),
            text: "t".into(),
        })
    }

    #[test]
    fn a_request_survives_json_and_runs_like_the_function() {
        let doc = email_doc("<p>{{ person.name }}</p>");
        let mut context = RenderContext::default();
        context.person.insert("name".into(), "Ann".into());
        let request = RenderRequest::Email {
            doc: Cow::Borrowed(&doc),
            context: Cow::Borrowed(&context),
            extras: Cow::default(),
        };
        let json = serde_json::to_vec(&request).unwrap();
        let back: RenderRequest<'static> = serde_json::from_slice(&json).unwrap();
        assert_eq!(back, request);

        let direct = crate::render_email(&doc, &context, &Hooks::untracked());
        let response = back.run();
        let wire = serde_json::to_vec(&response).unwrap();
        let response: RenderResponse = serde_json::from_slice(&wire).unwrap();
        assert_eq!(response.into_email(), Some(direct));
    }

    #[test]
    fn a_validate_request_survives_json_and_answers_with_the_issues_of_validate() {
        let doc = email_doc("{{ person.nope }}");
        let catalog = FieldCatalog::default();
        let request = RenderRequest::Validate {
            doc: Cow::Borrowed(&doc),
            catalog: Cow::Borrowed(&catalog),
            extras: Cow::default(),
        };
        let json = serde_json::to_vec(&request).unwrap();
        let back: RenderRequest<'static> = serde_json::from_slice(&json).unwrap();
        assert_eq!(back, request);

        let wire = serde_json::to_vec(&back.run()).unwrap();
        let response: RenderResponse = serde_json::from_slice(&wire).unwrap();
        let issues = response.into_issues().unwrap().unwrap();
        assert_eq!(issues, crate::validate(&doc, &catalog));
        assert!(issues.iter().any(|issue| issue.is_blocking()));
        assert_eq!(
            RenderResponse::Issues { issues: Vec::new() }.into_email(),
            None
        );
    }

    #[test]
    fn a_validate_request_for_a_document_no_type_owns_is_the_typed_refusal() {
        let mut blocks = email_doc("Hi");
        blocks.mode = crate::document::Mode::Blocks;
        let catalog = FieldCatalog::default();
        let request = RenderRequest::Validate {
            doc: Cow::Borrowed(&blocks),
            catalog: Cow::Borrowed(&catalog),
            extras: Cow::default(),
        };

        let wire = serde_json::to_vec(&request.run()).unwrap();
        let response: RenderResponse = serde_json::from_slice(&wire).unwrap();

        assert_eq!(
            response.into_issues(),
            Some(Err(ContentError::UnsupportedDocument))
        );
    }

    #[test]
    fn a_renderer_error_comes_back_typed() {
        let doc = email_doc("{{ nope.nothing }}");
        let request = RenderRequest::Email {
            doc: Cow::Owned(doc),
            context: Cow::Owned(RenderContext::default()),
            extras: Cow::default(),
        };
        let response = request.run();
        assert!(matches!(response, RenderResponse::Error { .. }));
        assert_eq!(
            response.into_email(),
            Some(Err(RenderError::Undefined { field: "html" }))
        );
    }

    #[test]
    fn the_wrong_kind_or_an_unknown_code_is_not_an_answer() {
        let webhook = RenderResponse::Webhook(RenderedWebhook {
            headers: Vec::new(),
            body: "{}".into(),
        });
        assert_eq!(webhook.into_email(), None);
        let unknown = RenderResponse::Error {
            code: "made_up".into(),
            field: None,
        };
        assert_eq!(unknown.clone().into_email(), None);
        assert_eq!(unknown.into_webhook(), None);
    }
}
