//! The wire shape of an isolated render (architecture 11.9): what the render host reads and
//! writes, so this pure crate owns the contract and the host and its pool only move bytes.
//!
//! A request carries the render call and its inputs; a response carries the result or the
//! renderer's typed error. Click tracking is not part of a request: the caller's token function
//! cannot cross a process boundary, so the host renders without tracking and the caller applies
//! [`crate::apply_tracking`] to the result.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::context::RenderContext;
use crate::document::TemplateDocument;
use crate::error::RenderError;
use crate::links::Hooks;
use crate::render::{Rendered, RenderedWebhook, render_email, render_webhook};

/// One render call.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RenderRequest<'a> {
    Email {
        doc: Cow<'a, TemplateDocument>,
        context: Cow<'a, RenderContext>,
    },
    Webhook {
        doc: Cow<'a, TemplateDocument>,
        context: Cow<'a, RenderContext>,
    },
}

/// The answer to one [`RenderRequest`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RenderResponse {
    Email(Rendered),
    Webhook(RenderedWebhook),
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
            Self::Email { doc, context } => {
                RenderResponse::from_email(render_email(doc, context, &Hooks::untracked()))
            }
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

    /// The email result, or `None` when this response is not an answer to an email request (or
    /// names an error code this crate does not define): a protocol fault.
    pub fn into_email(self) -> Option<Result<Rendered, RenderError>> {
        match self {
            Self::Email(rendered) => Some(Ok(rendered)),
            Self::Error { code, field } => RenderError::from_wire(&code, field.as_deref()).map(Err),
            Self::Webhook(_) => None,
        }
    }

    /// The webhook result, or `None` as for [`into_email`](Self::into_email).
    pub fn into_webhook(self) -> Option<Result<RenderedWebhook, RenderError>> {
        match self {
            Self::Webhook(rendered) => Some(Ok(rendered)),
            Self::Error { code, field } => RenderError::from_wire(&code, field.as_deref()).map(Err),
            Self::Email(_) => None,
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
        };
        let json = serde_json::to_vec(&request).unwrap();
        let back: RenderRequest<'static> = serde_json::from_slice(&json).unwrap();
        assert_eq!(back, request);

        let direct = render_email(&doc, &context, &Hooks::untracked());
        let response = back.run();
        let wire = serde_json::to_vec(&response).unwrap();
        let response: RenderResponse = serde_json::from_slice(&wire).unwrap();
        assert_eq!(response.into_email(), Some(direct));
    }

    #[test]
    fn a_renderer_error_comes_back_typed() {
        let doc = email_doc("{{ nope.nothing }}");
        let request = RenderRequest::Email {
            doc: Cow::Owned(doc),
            context: Cow::Owned(RenderContext::default()),
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
