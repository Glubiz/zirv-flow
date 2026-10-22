//! The child side: a synchronous, single-threaded loop that reads one framed request at a time,
//! renders it and writes one framed response. There is no async runtime here, and nothing is
//! written to standard output but frames; logs go to standard error.

use std::fmt;
use std::io::{self, Read, Write};

use tracing::error;
use zirv_flow_templates::{RenderRequest, RenderResponse};

use crate::frame::{FrameError, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, read_frame, write_frame};
use crate::limit;

/// The frame a child writes once it has applied its limit and is about to serve: the pool waits for
/// it before it hands the child a render, so a child that cannot start is found at start.
pub(crate) const READY: &[u8] = br#"{"kind":"ready"}"#;

/// Why the serve loop stopped before the input ended.
#[derive(Debug)]
pub enum HostError {
    /// A request frame was cut, over the bound, or the stream failed.
    Frame(String),
    /// A request frame did not hold a request.
    BadRequest,
    /// The response could not be written.
    Write(io::Error),
}

impl fmt::Display for HostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Frame(what) => write!(formatter, "bad request frame: {what}"),
            Self::BadRequest => formatter.write_str("the request frame is not a render request"),
            Self::Write(error) => write!(formatter, "could not write the response: {error}"),
        }
    }
}

impl std::error::Error for HostError {}

/// Serve requests from `input` to `output` until `input` ends, which is a clean stop.
///
/// A frame that cannot be trusted ends the loop with an error: the stream cannot be re-aligned,
/// and the pool replaces the child.
pub fn serve(mut input: impl Read, mut output: impl Write) -> Result<(), HostError> {
    loop {
        let frame = match read_frame(&mut input, MAX_REQUEST_BYTES) {
            Ok(frame) => frame,
            Err(FrameError::Closed) => return Ok(()),
            Err(FrameError::Truncated) => return Err(HostError::Frame("cut short".into())),
            Err(FrameError::TooLarge(length)) => {
                return Err(HostError::Frame(format!(
                    "{length} bytes is over the bound"
                )));
            }
            Err(FrameError::Unexpected) => return Err(HostError::BadRequest),
            Err(FrameError::Io(error)) => return Err(HostError::Frame(error.to_string())),
        };
        let request: RenderRequest<'_> =
            serde_json::from_slice(&frame).map_err(|_| HostError::BadRequest)?;
        let response = request.run();
        let mut bytes = serde_json::to_vec(&response).map_err(|_| HostError::BadRequest)?;
        if bytes.len() > MAX_RESPONSE_BYTES {
            // The renderer's caps keep this out of reach; the bound still holds if they change.
            bytes = serde_json::to_vec(&RenderResponse::Error {
                code: "output_too_large".into(),
                field: None,
            })
            .map_err(|_| HostError::BadRequest)?;
        }
        write_frame(&mut output, &bytes).map_err(HostError::Write)?;
    }
}

/// The child's whole life: limit memory first, announce it is ready, then serve standard input to standard output.
/// Returns the process exit code.
pub fn run(memory_limit_bytes: Option<u64>) -> u8 {
    if let Err(error) = limit::apply(memory_limit_bytes) {
        error!(%error, "the render host could not limit its memory");
        return 1;
    }
    if let Err(error) = write_frame(&mut io::stdout().lock(), READY) {
        error!(%error, "the render host could not announce itself");
        return 1;
    }
    match serve(io::stdin().lock(), io::stdout().lock()) {
        Ok(()) => 0,
        Err(error) => {
            error!(%error, "the render host stopped");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use zirv_flow_templates::{
        Category, EmailContent, Hooks, RenderContext, TemplateDocument, render_email,
    };

    use super::*;
    use crate::frame::write_frame;

    fn doc() -> TemplateDocument {
        TemplateDocument::email_code(EmailContent {
            subject: "Hi {{ person.name }}".into(),
            sender: None,
            lang: None,
            category: Category::Transactional,
            html: "<p>{{ person.name }}</p>".into(),
            text: "Hi".into(),
        })
    }

    fn context(name: &str) -> RenderContext {
        let mut context = RenderContext::default();
        context.person.insert("name".into(), name.into());
        context
    }

    fn request_frame(doc: &TemplateDocument, context: &RenderContext) -> Vec<u8> {
        let request = RenderRequest::Email {
            doc: Cow::Borrowed(doc),
            context: Cow::Borrowed(context),
        };
        let mut out = Vec::new();
        write_frame(&mut out, &serde_json::to_vec(&request).unwrap()).unwrap();
        out
    }

    fn responses(mut output: &[u8]) -> Vec<RenderResponse> {
        let mut found = Vec::new();
        while let Ok(frame) = read_frame(&mut output, MAX_RESPONSE_BYTES) {
            found.push(serde_json::from_slice(&frame).unwrap());
        }
        found
    }

    #[test]
    fn it_answers_each_request_in_order_and_stops_when_the_input_ends() {
        let doc = doc();
        let mut input = request_frame(&doc, &context("Ann"));
        input.extend(request_frame(&doc, &context("Bo")));
        let mut output = Vec::new();
        serve(input.as_slice(), &mut output).unwrap();

        let answers = responses(&output);
        assert_eq!(answers.len(), 2);
        for (answer, name) in answers.into_iter().zip(["Ann", "Bo"]) {
            let direct = render_email(&doc, &context(name), &Hooks::untracked());
            assert_eq!(answer.into_email(), Some(direct));
        }
    }

    #[test]
    fn it_serves_a_validate_request_with_the_issues_of_validate() {
        let doc = doc();
        let catalog = zirv_flow_templates::FieldCatalog::default();
        let request = RenderRequest::Validate {
            doc: Cow::Borrowed(&doc),
            catalog: Cow::Borrowed(&catalog),
        };
        let mut input = Vec::new();
        write_frame(&mut input, &serde_json::to_vec(&request).unwrap()).unwrap();
        let mut output = Vec::new();
        serve(input.as_slice(), &mut output).unwrap();

        let issues = responses(&output).remove(0).into_issues().unwrap();
        assert_eq!(issues, zirv_flow_templates::validate(&doc, &catalog));
    }

    #[test]
    fn the_renderers_error_is_an_answer_not_a_failure_of_the_host() {
        let bad = TemplateDocument::email_code(EmailContent {
            html: "{{ nope.nothing }}".into(),
            ..EmailContent::default()
        });
        let input = request_frame(&bad, &RenderContext::default());
        let mut output = Vec::new();
        serve(input.as_slice(), &mut output).unwrap();
        let answers = responses(&output);
        assert!(matches!(answers[0], RenderResponse::Error { .. }));
    }

    #[test]
    fn untrusted_frames_end_the_loop_with_an_error() {
        let mut garbage = Vec::new();
        write_frame(&mut garbage, b"not json").unwrap();
        assert!(matches!(
            serve(garbage.as_slice(), Vec::new()),
            Err(HostError::BadRequest)
        ));

        let oversized = [0xff, 0xff, 0xff, 0xff];
        assert!(matches!(
            serve(oversized.as_slice(), Vec::new()),
            Err(HostError::Frame(_))
        ));

        let whole = request_frame(&doc(), &context("Ann"));
        assert!(matches!(
            serve(&whole[..whole.len() - 3], Vec::new()),
            Err(HostError::Frame(_))
        ));
    }

    #[test]
    fn a_failing_output_is_reported() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let input = request_frame(&doc(), &context("Ann"));
        assert!(matches!(
            serve(input.as_slice(), Broken),
            Err(HostError::Write(_))
        ));
    }
}
