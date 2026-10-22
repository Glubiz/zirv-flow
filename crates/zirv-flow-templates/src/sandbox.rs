//! The MiniJinja sandbox: no filesystem or network, strict undefined, fuel, a recursion limit
//! and a capped output (architecture 11.4).
//!
//! The environment loads no templates from anywhere: every template is a string passed in, with
//! no loader, so `include`, `extends` and `import` find nothing (the `multi_template`
//! feature is on only because the `fuel` feature does not compile without it in 2.24). The
//! only globals are `range`, `dict` and `namespace`; `debug` is removed because it prints the
//! whole context. The builtin filters read no clock and draw no random numbers.

use std::io::{self, Write};

use minijinja::{AutoEscape, Environment, Error, ErrorKind, UndefinedBehavior, Value};

use crate::error::RenderError;

/// Fuel for one field's render. One unit is roughly one VM instruction, so this allows about two
/// hundred thousand: a long personalised newsletter is a few thousand, and a nested loop that
/// runs away is stopped within milliseconds.
pub const FUEL: u64 = 200_000;
/// Nesting depth of loops, macros and conditionals. MiniJinja's own ceiling is 500; a lower
/// value keeps a hostile template far from the thread's stack limit.
pub const RECURSION_LIMIT: usize = 64;
/// The longest template source accepted, per field. 256 KiB is far beyond real email HTML, and
/// bounds compile time and memory.
pub const MAX_SOURCE_BYTES: usize = 256 * 1024;
/// The longest rendered HTML part. Mail clients clip near 100 KB; 512 KiB is the hard stop
/// that still lets the validator warn well before it.
pub const MAX_HTML_BYTES: usize = 512 * 1024;
/// The longest rendered text part.
pub const MAX_TEXT_BYTES: usize = 256 * 1024;
/// The longest rendered subject. RFC 5322 caps a header line at 998 octets.
pub const MAX_SUBJECT_BYTES: usize = 998;
/// The longest rendered header value (webhook headers), the usual HTTP server limit.
pub const MAX_HEADER_VALUE_BYTES: usize = 4096;
/// The longest webhook body, rendered. Matches the size a receiver reasonably accepts.
pub const MAX_WEBHOOK_BODY_BYTES: usize = 256 * 1024;

/// The global functions a template may call (and so the roots `validate` accepts besides the
/// context sections).
pub(crate) const GLOBAL_FUNCTIONS: [&str; 3] = ["range", "dict", "namespace"];

/// The sandboxed environment. Templates named `*.html` are autoescaped, all others are not.
pub(crate) fn environment() -> Environment<'static> {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env.set_fuel(Some(FUEL));
    env.set_recursion_limit(RECURSION_LIMIT);
    env.set_keep_trailing_newline(true);
    env.remove_global("debug");
    env.set_auto_escape_callback(|name| {
        if name.ends_with(".html") {
            AutoEscape::Html
        } else {
            AutoEscape::None
        }
    });
    env
}

/// A writer that refuses bytes past a limit instead of growing without bound.
pub(crate) struct CappedWriter {
    buffer: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl CappedWriter {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            buffer: Vec::new(),
            limit,
            exceeded: false,
        }
    }

    pub(crate) fn exceeded(&self) -> bool {
        self.exceeded
    }

    pub(crate) fn into_string(self) -> Option<String> {
        String::from_utf8(self.buffer).ok()
    }
}

impl Write for CappedWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.buffer.len().saturating_add(data.len()) > self.limit {
            self.exceeded = true;
            return Err(io::Error::other("output limit reached"));
        }
        self.buffer.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Map a MiniJinja error to a typed error. Nothing from the error text is kept.
pub(crate) fn map_error(error: &Error, field: &'static str, exceeded: bool) -> RenderError {
    if exceeded {
        return RenderError::OutputTooLarge { field };
    }
    match error.kind() {
        ErrorKind::SyntaxError => RenderError::Syntax { field },
        ErrorKind::UndefinedError => RenderError::Undefined { field },
        ErrorKind::OutOfFuel => RenderError::OutOfFuel { field },
        ErrorKind::InvalidOperation if error.detail() == Some("recursion limit exceeded") => {
            RenderError::RecursionLimit { field }
        }
        _ => RenderError::Template { field },
    }
}

/// Render one template string to at most `limit` bytes. `name` decides escaping: it must end in
/// `.html` for an HTML field.
pub(crate) fn render_string(
    env: &Environment<'static>,
    name: &'static str,
    field: &'static str,
    source: &str,
    context: &Value,
    limit: usize,
) -> Result<String, RenderError> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(RenderError::SourceTooLarge { field });
    }
    let template = env
        .template_from_named_str(name, source)
        .map_err(|error| map_error(&error, field, false))?;
    let mut writer = CappedWriter::new(limit);
    match template.render_captured_to(context, &mut writer) {
        Ok(_) => writer.into_string().ok_or(RenderError::Template { field }),
        Err(error) => Err(map_error(&error, field, writer.exceeded())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(name: &'static str, source: &str, limit: usize) -> Result<String, RenderError> {
        let context = Value::from_serialize(serde_json::json!({
            "person": { "name": "<b>Ann</b>", "tags": ["a", "b"] }
        }));
        render_string(&environment(), name, "html", source, &context, limit)
    }

    #[test]
    fn html_is_escaped_in_html_fields_only() {
        assert_eq!(
            render("t.html", "{{ person.name }}", 1000).unwrap(),
            "&lt;b&gt;Ann&lt;&#x2f;b&gt;"
        );
        assert_eq!(
            render("t.txt", "{{ person.name }}", 1000).unwrap(),
            "<b>Ann</b>"
        );
    }

    #[test]
    fn undefined_is_an_error() {
        assert_eq!(
            render("t.txt", "{{ person.nope }}", 1000),
            Err(RenderError::Undefined { field: "html" })
        );
        assert_eq!(
            render("t.txt", "{{ nobody.name }}", 1000),
            Err(RenderError::Undefined { field: "html" })
        );
        assert_eq!(
            render("t.txt", "{{ person.nope | default('x') }}", 1000).unwrap(),
            "x"
        );
    }

    #[test]
    fn runaway_loops_run_out_of_fuel() {
        let source =
            "{% for a in range(1000) %}{% for b in range(1000) %}{{ a }}{% endfor %}{% endfor %}";
        assert_eq!(
            render("t.txt", source, usize::MAX),
            Err(RenderError::OutOfFuel { field: "html" })
        );
    }

    #[test]
    fn deep_recursion_hits_the_limit() {
        let source = "{% macro f(n) %}{{ f(n + 1) }}{% endmacro %}{{ f(0) }}";
        assert_eq!(
            render("t.txt", source, 1000),
            Err(RenderError::RecursionLimit { field: "html" })
        );
    }

    #[test]
    fn oversized_output_is_refused() {
        assert_eq!(
            render("t.txt", "{{ 'abcdefghij' }}", 5),
            Err(RenderError::OutputTooLarge { field: "html" })
        );
        assert_eq!(render("t.txt", "{{ 'abcde' }}", 5).unwrap(), "abcde");
    }

    #[test]
    fn oversized_source_is_refused() {
        let source = "a".repeat(MAX_SOURCE_BYTES + 1);
        assert_eq!(
            render("t.txt", &source, usize::MAX),
            Err(RenderError::SourceTooLarge { field: "html" })
        );
    }

    #[test]
    fn syntax_errors_are_typed() {
        assert_eq!(
            render("t.txt", "{% if %}", 100),
            Err(RenderError::Syntax { field: "html" })
        );
    }

    #[test]
    fn no_filesystem_or_debug_dump() {
        assert!(render("t.txt", "{% include 'x' %}", 100).is_err());
        assert!(render("t.txt", "{{ debug() }}", 100).is_err());
    }

    #[test]
    fn trailing_newline_is_kept() {
        assert_eq!(render("t.txt", "a\n", 100).unwrap(), "a\n");
    }
}
