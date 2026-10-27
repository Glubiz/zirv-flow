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
/// The most webhook headers a document may define; real integrations use a handful, and it bounds
/// the work and the request size.
pub const MAX_HEADER_COUNT: usize = 32;
/// The most bytes in all header names and rendered values together, the usual 16 KiB limit a
/// receiver puts on a request header block.
pub const MAX_HEADERS_BYTES: usize = 16 * 1024;
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

/// Whether `name` is an RFC 9110 `token`: non-empty, and only the characters allowed in a header
/// field name. This keeps a CR, LF, `:` or space out of a name.
pub(crate) fn is_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
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
    render_raw(env, name, source, context, limit)
        .map_err(|(error, exceeded)| map_error(&error, field, exceeded))?
        .ok_or(RenderError::Template { field })
}

/// Render one template string straight into a [`CappedWriter`], so the output never holds more
/// than `limit` bytes: the render stops at the first byte past it. On failure returns the
/// MiniJinja error and whether the cap was the cause. `Ok(None)` is output that is not UTF-8.
pub(crate) fn render_raw(
    env: &Environment<'static>,
    name: &str,
    source: &str,
    context: &Value,
    limit: usize,
) -> Result<Option<String>, (Error, bool)> {
    let template = env
        .template_from_named_str(name, source)
        .map_err(|error| (error, false))?;
    let mut writer = CappedWriter::new(limit);
    match template.render_captured_to(context, &mut writer) {
        Ok(_) => Ok(writer.into_string()),
        Err(error) => Err((error, writer.exceeded())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Category, EmailContent, FieldCatalog, TemplateDocument, validate};

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
    fn a_loop_past_the_cap_stops_at_the_cap() {
        // Far more output than the cap (and than the fuel allows): the render must fail with the
        // cap error, and the writer must never have held more than the cap.
        let source = "{% for i in range(10000) %}0123456789{% endfor %}";
        assert_eq!(
            render("t.txt", source, 50),
            Err(RenderError::OutputTooLarge { field: "html" })
        );
        let context = Value::from_serialize(serde_json::json!({}));
        let (_, exceeded) = render_raw(&environment(), "t.txt", source, &context, 50).unwrap_err();
        assert!(exceeded);
    }

    #[test]
    fn header_names_are_tokens() {
        for good in ["X-A", "a", "X_b.c~1", "!#$%&'*+-.^_`|~"] {
            assert!(is_header_name(good), "{good}");
        }
        for bad in [
            "",
            "X A",
            "X:A",
            "X-A
X-Evil: 1",
            "X/B",
            "X\u{e9}",
            "X-A
",
        ] {
            assert!(!is_header_name(bad), "{bad:?}");
        }
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
    fn sandbox_escape_table_is_typed_or_bounded_and_checked_by_the_editor() {
        use RenderError::{
            OutOfFuel, OutputTooLarge, RecursionLimit, SourceTooLarge, Syntax, Template, Undefined,
        };

        let identifier = "x".repeat(4096);
        let long_identifier = format!("{{% set {identifier} = 1 %}}{{{{ {identifier} }}}}");
        let oversized_identifier = format!("{{{{ {} }}}}", "x".repeat(MAX_SOURCE_BYTES));
        let cases = [
            (
                "include",
                "{% include '/etc/passwd' %}",
                Err(Template { field: "html" }),
            ),
            (
                "import",
                "{% import '/etc/passwd' as file %}",
                Err(Template { field: "html" }),
            ),
            (
                "from import",
                "{% from '/etc/passwd' import secret %}",
                Err(Template { field: "html" }),
            ),
            (
                "extends",
                "{% extends '/etc/passwd' %}{% block body %}x{% endblock %}",
                Err(Template { field: "html" }),
            ),
            ("block", "{% block body %}ok{% endblock %}", Ok("ok")),
            (
                "recursive block and self",
                "{% block body %}{{ self.body() }}{% endblock %}",
                Err(RecursionLimit { field: "html" }),
            ),
            (
                "recursive macro",
                "{% macro f(n) %}{{ f(n + 1) }}{% endmacro %}{{ f(0) }}",
                Err(RecursionLimit { field: "html" }),
            ),
            (
                "recursive call",
                "{% macro f() %}{{ caller() }}{% call f() %}x{% endcall %}{% endmacro %}{% call f() %}x{% endcall %}",
                Err(RecursionLimit { field: "html" }),
            ),
            (
                "huge range",
                "{% for i in range(10**9) %}x{% endfor %}",
                Err(Template { field: "html" }),
            ),
            (
                "nested ranges",
                "{% for a in range(1000) %}{% for b in range(1000) %}{{ a }}{% endfor %}{% endfor %}",
                Err(OutOfFuel { field: "html" }),
            ),
            (
                "dict growth",
                "{% set ns = namespace(d=dict()) %}{% for i in range(16) %}{% set ns.d = dict(ns.d, **{('k' ~ i): i}) %}{% endfor %}{{ ns.d | length }}",
                Ok("16"),
            ),
            (
                "namespace growth",
                "{% set ns = namespace(s='') %}{% for i in range(32) %}{% set ns.s = ns.s ~ 'x' %}{% endfor %}{{ ns.s | length }}",
                Ok("32"),
            ),
            // Keep intermediate allocation small here; unbounded doubling belongs in the OS-isolated API test.
            (
                "string doubling",
                "{% set ns = namespace(s='x') %}{% for i in range(20) %}{% set ns.s = ns.s ~ ns.s %}{% endfor %}{{ ns.s }}",
                Err(OutputTooLarge { field: "html" }),
            ),
            ("long identifier", long_identifier.as_str(), Ok("1")),
            (
                "oversized identifier",
                oversized_identifier.as_str(),
                Err(SourceTooLarge { field: "html" }),
            ),
            (
                "BOM and Unicode text",
                "\u{feff}{{ 'こんにちは 🌍' }}",
                Ok("\u{feff}こんにちは 🌍"),
            ),
            (
                "Unicode identifier",
                "{{ café }}",
                Err(Syntax { field: "html" }),
            ),
            ("lipsum", "{{ lipsum() }}", Err(Template { field: "html" })),
            ("debug", "{{ debug() }}", Err(Template { field: "html" })),
            (
                "self lookup",
                "{{ self.secret }}",
                Err(Undefined { field: "html" }),
            ),
            (
                "loop outside a loop",
                "{{ loop.index }}",
                Err(Undefined { field: "html" }),
            ),
            (
                "recursive loop",
                "{% for i in [1] recursive %}{{ loop([1]) }}{% endfor %}",
                Err(RecursionLimit { field: "html" }),
            ),
            (
                "loop method abuse",
                "{% for i in [1] %}{{ loop.__class__() }}{% endfor %}",
                Err(Template { field: "html" }),
            ),
            (
                "unknown filter",
                "{{ 'x' | filesystem }}",
                Err(Template { field: "html" }),
            ),
            (
                "unavailable random filter",
                "{{ [1, 2] | random }}",
                Err(Template { field: "html" }),
            ),
        ];
        for (name, source, expected) in cases {
            let result = render("t.html", source, MAX_HTML_BYTES);
            assert_eq!(result.as_deref(), expected.as_ref().copied(), "{name}");
            if let Ok(output) = &result {
                assert!(output.len() <= MAX_HTML_BYTES, "{name}");
            }
            let doc = TemplateDocument::email_code(EmailContent {
                subject: "Sandbox probe".into(),
                sender: Some("00000000-0000-7000-8000-000000000001".into()),
                lang: Some("en".into()),
                category: Category::Transactional,
                syntax: Default::default(),
                html: source.into(),
                text: "Sandbox probe".into(),
            });
            let issues = validate(&doc, &FieldCatalog::default());
            if expected.is_err() {
                assert!(
                    issues.iter().any(|issue| issue.at == "/email/html"),
                    "{name}: {issues:?}"
                );
            } else {
                assert!(issues.is_empty(), "{name}: {issues:?}");
            }
        }
    }

    #[test]
    fn trailing_newline_is_kept() {
        assert_eq!(render("t.txt", "a\n", 100).unwrap(), "a\n");
    }
}
