//! Why a render failed: a fixed code and the field concerned, never template or person data.

use std::fmt;

/// A render failed. No variant carries a value from the template or the context, so an error is
/// safe to log and to return to a marketer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderError {
    /// The document's schema is not the one this crate reads.
    UnsupportedSchema,
    /// The document is not code-mode content for the function called.
    WrongDocument,
    /// A source is longer than [`crate::MAX_SOURCE_BYTES`].
    SourceTooLarge { field: &'static str },
    /// A MiniJinja syntax error.
    Syntax { field: &'static str },
    /// A variable or attribute is undefined (strict undefined).
    Undefined { field: &'static str },
    /// The fuel limit was reached.
    OutOfFuel { field: &'static str },
    /// The recursion limit was reached.
    RecursionLimit { field: &'static str },
    /// The output would exceed its limit.
    OutputTooLarge { field: &'static str },
    /// Any other template error.
    Template { field: &'static str },
    /// The webhook body is not valid JSON as written.
    InvalidBody,
    /// A webhook body object key holds a template.
    TemplateInKey,
    /// The rendered webhook body is over [`crate::MAX_WEBHOOK_BODY_BYTES`].
    BodyTooLarge,
    /// A webhook header name is not an RFC 9110 token.
    InvalidHeaderName,
    /// More than [`crate::MAX_HEADER_COUNT`] webhook headers.
    TooManyHeaders,
    /// Header names and values together are over [`crate::MAX_HEADERS_BYTES`].
    HeadersTooLarge,
    /// A marketing email's HTML has no link to the message's unsubscribe URL.
    MissingUnsubscribe,
    /// The message's unsubscribe URL is not an `https` URL (RFC 8058).
    InvalidUnsubscribeUrl,
    /// The MJML source holds an `mj-include` element, which is never loaded.
    MjmlInclude,
    /// `mrml` could not parse or render the MJML.
    Mjml,
    /// The MJML nests deeper than `mrml` is given.
    MjmlTooDeep,
    /// A block document the checks refuse, or a rendered URL a link may not use.
    InvalidBlocks,
}

impl RenderError {
    /// A fixed machine-readable code.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedSchema => "unsupported_schema",
            Self::WrongDocument => "wrong_document",
            Self::SourceTooLarge { .. } => "source_too_large",
            Self::Syntax { .. } => "syntax_error",
            Self::Undefined { .. } => "undefined_value",
            Self::OutOfFuel { .. } => "out_of_fuel",
            Self::RecursionLimit { .. } => "recursion_limit",
            Self::OutputTooLarge { .. } => "output_too_large",
            Self::Template { .. } => "template_error",
            Self::InvalidBody => "invalid_json_body",
            Self::TemplateInKey => "template_in_key",
            Self::BodyTooLarge => "body_too_large",
            Self::InvalidHeaderName => "invalid_header_name",
            Self::TooManyHeaders => "too_many_headers",
            Self::HeadersTooLarge => "headers_too_large",
            Self::MissingUnsubscribe => "missing_unsubscribe",
            Self::InvalidUnsubscribeUrl => "invalid_unsubscribe_url",
            Self::MjmlInclude => "mjml_include",
            Self::Mjml => "mjml_error",
            Self::MjmlTooDeep => "mjml_too_deep",
            Self::InvalidBlocks => "invalid_blocks",
        }
    }

    /// The error with this [`code`](Self::code) and [`field`](Self::field), as read back from a
    /// response of the render host. `None` for a code this crate does not define.
    pub fn from_wire(code: &str, field: Option<&str>) -> Option<Self> {
        let field = match field {
            Some("subject") => "subject",
            Some("html") => "html",
            Some("text") => "text",
            Some("headers") => "headers",
            Some("body") => "body",
            _ => "unknown",
        };
        Some(match code {
            "unsupported_schema" => Self::UnsupportedSchema,
            "wrong_document" => Self::WrongDocument,
            "source_too_large" => Self::SourceTooLarge { field },
            "syntax_error" => Self::Syntax { field },
            "undefined_value" => Self::Undefined { field },
            "out_of_fuel" => Self::OutOfFuel { field },
            "recursion_limit" => Self::RecursionLimit { field },
            "output_too_large" => Self::OutputTooLarge { field },
            "template_error" => Self::Template { field },
            "invalid_json_body" => Self::InvalidBody,
            "template_in_key" => Self::TemplateInKey,
            "body_too_large" => Self::BodyTooLarge,
            "invalid_header_name" => Self::InvalidHeaderName,
            "too_many_headers" => Self::TooManyHeaders,
            "headers_too_large" => Self::HeadersTooLarge,
            "missing_unsubscribe" => Self::MissingUnsubscribe,
            "invalid_unsubscribe_url" => Self::InvalidUnsubscribeUrl,
            "mjml_include" => Self::MjmlInclude,
            "mjml_error" => Self::Mjml,
            "mjml_too_deep" => Self::MjmlTooDeep,
            "invalid_blocks" => Self::InvalidBlocks,
            _ => return None,
        })
    }

    /// The field the error concerns (`subject`, `html`, `text`, `headers`, `body`), if any.
    pub const fn field(&self) -> Option<&'static str> {
        match self {
            Self::SourceTooLarge { field }
            | Self::Syntax { field }
            | Self::Undefined { field }
            | Self::OutOfFuel { field }
            | Self::RecursionLimit { field }
            | Self::OutputTooLarge { field }
            | Self::Template { field } => Some(*field),
            _ => None,
        }
    }
}

impl fmt::Display for RenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.field() {
            Some(field) => write!(formatter, "{} in {field}", self.code()),
            None => formatter.write_str(self.code()),
        }
    }
}

impl std::error::Error for RenderError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_code_and_field_only() {
        assert_eq!(
            RenderError::Undefined { field: "html" }.to_string(),
            "undefined_value in html"
        );
        assert_eq!(RenderError::BodyTooLarge.to_string(), "body_too_large");
        assert_eq!(RenderError::InvalidBody.field(), None);
    }

    #[test]
    fn every_error_survives_its_wire_form() {
        let all = [
            RenderError::UnsupportedSchema,
            RenderError::WrongDocument,
            RenderError::SourceTooLarge { field: "body" },
            RenderError::Syntax { field: "subject" },
            RenderError::Undefined { field: "html" },
            RenderError::OutOfFuel { field: "text" },
            RenderError::RecursionLimit { field: "headers" },
            RenderError::OutputTooLarge { field: "html" },
            RenderError::Template { field: "body" },
            RenderError::InvalidBody,
            RenderError::TemplateInKey,
            RenderError::BodyTooLarge,
            RenderError::InvalidHeaderName,
            RenderError::TooManyHeaders,
            RenderError::HeadersTooLarge,
            RenderError::MissingUnsubscribe,
            RenderError::InvalidUnsubscribeUrl,
            RenderError::MjmlInclude,
            RenderError::Mjml,
            RenderError::MjmlTooDeep,
            RenderError::InvalidBlocks,
        ];
        for error in all {
            assert_eq!(
                RenderError::from_wire(error.code(), error.field()),
                Some(error)
            );
        }
        assert_eq!(RenderError::from_wire("nope", None), None);
    }
}
