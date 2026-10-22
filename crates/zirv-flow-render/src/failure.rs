//! Why an isolated render failed.

use std::fmt;

use zirv_flow_templates::RenderError;

/// A render through the pool failed. Only [`Render`](Self::Render) is the renderer's own verdict
/// on a document; every other variant is the render host failing, and costs that one render only:
/// the pool kills the child, starts another, and the worker and the API carry on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderFailure {
    /// The renderer refused the document or the context (a syntax error, a limit, ...).
    Render(RenderError),
    /// The child ran out of address space (`RLIMIT_AS`) and aborted on a failed allocation.
    MemoryLimit,
    /// The child did not answer within the per-render deadline and was killed.
    Deadline,
    /// The child exited, crashed or was killed without the memory limit being the cause, or could
    /// not be started.
    ChildExited,
    /// The child sent something that is not a valid response: an oversized, cut or malformed
    /// frame, or a response of the wrong kind.
    Protocol,
    /// A validate request for a document no registered content type owns.
    UnsupportedDocument,
    /// The request is over the frame bound, so it was not sent.
    RequestTooLarge,
}

impl RenderFailure {
    /// A fixed machine-readable reason; also the `reason` label of the failure metric.
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::Render(_) => "render_error",
            Self::MemoryLimit => "memory_limit",
            Self::Deadline => "deadline",
            Self::ChildExited => "child_exited",
            Self::Protocol => "protocol",
            Self::RequestTooLarge => "request_too_large",
            Self::UnsupportedDocument => "unsupported_document",
        }
    }
}

impl fmt::Display for RenderFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Render(error) => write!(formatter, "render_error: {error}"),
            other => formatter.write_str(other.reason()),
        }
    }
}

impl std::error::Error for RenderFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Render(error) => Some(error),
            _ => None,
        }
    }
}

impl From<RenderError> for RenderFailure {
    fn from(error: RenderError) -> Self {
        Self::Render(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_are_fixed_and_the_renderers_error_keeps_its_code() {
        let all = [
            (RenderFailure::MemoryLimit, "memory_limit"),
            (RenderFailure::Deadline, "deadline"),
            (RenderFailure::ChildExited, "child_exited"),
            (RenderFailure::Protocol, "protocol"),
            (RenderFailure::RequestTooLarge, "request_too_large"),
            (RenderFailure::UnsupportedDocument, "unsupported_document"),
            (
                RenderFailure::Render(RenderError::InvalidBody),
                "render_error",
            ),
        ];
        for (failure, reason) in all {
            assert_eq!(failure.reason(), reason);
        }
        assert_eq!(
            RenderFailure::from(RenderError::Syntax { field: "html" }).to_string(),
            "render_error: syntax_error in html"
        );
        assert_eq!(RenderFailure::Deadline.to_string(), "deadline");
    }
}
