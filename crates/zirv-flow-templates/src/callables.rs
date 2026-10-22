//! A static scan for the filters, tests and method calls a template names, whichever branch
//! runs. The sample render only sees the branches the sample takes; this sees all of them.
//!
//! The scan is a small lexer over the `{{ ... }}` and `{% ... %}` spans. It skips string
//! literals, `{# ... #}` comments and `{% raw %}` bodies, so text that only looks like a filter
//! is not read as one.
//!
//! Filter and test names are checked against the sandbox environment itself (a one-line probe
//! render), so the check never drifts from what is registered. Methods are refused outright:
//! the render context is plain JSON values, and MiniJinja gives a plain value no methods
//! without the `pycompat` callback (`Value::call_method` answers `UnknownMethod` for anything
//! that is not an object; the sandbox sets no unknown-method callback). The two objects a
//! template can reach that do have methods are `loop.cycle(..)` and `loop.changed(..)`.

use std::collections::BTreeSet;

use minijinja::{Environment, ErrorKind};

/// What was named, and where the name was used.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum Kind {
    Filter,
    Test,
    Method,
}

/// The loop object's methods, the only methods a template may call.
const LOOP_METHODS: [&str; 2] = ["cycle", "changed"];

#[derive(Debug, Eq, PartialEq)]
enum Token {
    Ident(String),
    Pipe,
    Dot,
    LParen,
    Other,
}

/// Every filter, test and method name in `source`, and whether it is unknown: sorted, one entry
/// per kind and name. A method is always listed (it is refused); filters and tests only when
/// the sandbox environment does not have them.
pub(crate) fn unknown_callables(env: &Environment<'static>, source: &str) -> Vec<(Kind, String)> {
    let mut found = BTreeSet::new();
    for (kind, name) in scan(source) {
        let unknown = match kind {
            Kind::Method => true,
            Kind::Filter => probe(
                env,
                &format!("{{{{ 1 | {name} }}}}"),
                ErrorKind::UnknownFilter,
            ),
            Kind::Test => probe(
                env,
                &format!("{{{{ 1 is {name} }}}}"),
                ErrorKind::UnknownTest,
            ),
        };
        if unknown {
            found.insert((kind, name));
        }
    }
    found.into_iter().collect()
}

/// Whether rendering the probe fails with `unknown`.
fn probe(env: &Environment<'static>, source: &str, unknown: ErrorKind) -> bool {
    env.render_str(source, ())
        .is_err_and(|error| error.kind() == unknown)
}

/// The `(kind, name)` pairs named in `source`, in order of first use.
fn scan(source: &str) -> Vec<(Kind, String)> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'{' {
            at += 1;
            continue;
        }
        match bytes.get(at + 1) {
            Some(b'#') => match source[at + 2..].find("#}") {
                Some(n) => at += 2 + n + 2,
                None => return out,
            },
            Some(&open @ (b'{' | b'%')) => {
                let (tokens, end) = tag_tokens(bytes, at + 2, open);
                at = end;
                let block = open == b'%';
                if block && tokens.first() == Some(&Token::Ident("raw".to_owned())) {
                    match raw_end(source, at) {
                        Some(end) => at = end,
                        None => return out,
                    }
                    continue;
                }
                analyse(&tokens, block, &mut out);
            }
            _ => at += 1,
        }
    }
    out
}

/// The position after `{% endraw %}` for a raw block whose body starts at `from`.
fn raw_end(source: &str, from: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut at = from;
    while let Some(n) = source[at..].find("{%") {
        let mut i = at + n + 2;
        if matches!(bytes.get(i), Some(b'-' | b'+')) {
            i += 1;
        }
        while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        if source[i..].starts_with("endraw") {
            return Some(source[i..].find("%}").map_or(source.len(), |n| i + n + 2));
        }
        at += n + 2;
    }
    None
}

/// Tokenise one tag from `from` (just after the opener) to its closer; returns the tokens and
/// the position after the closer. `open` is `{` for an expression and `%` for a block tag.
fn tag_tokens(bytes: &[u8], from: usize, open: u8) -> (Vec<Token>, usize) {
    let closer = if open == b'{' { b'}' } else { b'%' };
    let mut tokens = Vec::new();
    let mut depth = 0usize;
    let mut at = from;
    // A `-` or `+` right after the opener only trims whitespace.
    if matches!(bytes.get(at), Some(b'-' | b'+')) {
        at += 1;
    }
    while let Some(&byte) = bytes.get(at) {
        match byte {
            b'\'' | b'"' => {
                at = skip_string(bytes, at);
                tokens.push(Token::Other);
            }
            b if b == closer && bytes.get(at + 1) == Some(&b'}') && depth == 0 => {
                return (tokens, at + 2);
            }
            b'{' => {
                depth += 1;
                at += 1;
                tokens.push(Token::Other);
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                at += 1;
                tokens.push(Token::Other);
            }
            b'|' => {
                at += 1;
                tokens.push(Token::Pipe);
            }
            b'.' => {
                at += 1;
                tokens.push(Token::Dot);
            }
            b'(' => {
                at += 1;
                tokens.push(Token::LParen);
            }
            b if b.is_ascii_alphabetic() || b == b'_' => {
                let start = at;
                while bytes
                    .get(at)
                    .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
                {
                    at += 1;
                }
                let name = String::from_utf8_lossy(&bytes[start..at]).into_owned();
                tokens.push(Token::Ident(name));
            }
            b if b.is_ascii_digit() => {
                // A number: `1.5` is one token, so its dot is not a method dot.
                while bytes
                    .get(at)
                    .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
                {
                    at += 1;
                }
                if bytes.get(at) == Some(&b'.') && bytes.get(at + 1).is_some_and(u8::is_ascii_digit)
                {
                    at += 1;
                    while bytes.get(at).is_some_and(u8::is_ascii_alphanumeric) {
                        at += 1;
                    }
                }
                tokens.push(Token::Other);
            }
            b if b.is_ascii_whitespace() => at += 1,
            _ => {
                at += 1;
                tokens.push(Token::Other);
            }
        }
    }
    (tokens, bytes.len())
}

/// The position after the string literal that starts at `at` (a quote), honouring `\` escapes.
fn skip_string(bytes: &[u8], at: usize) -> usize {
    let quote = bytes[at];
    let mut i = at + 1;
    while let Some(&byte) = bytes.get(i) {
        match byte {
            b'\\' => i += 2,
            b if b == quote => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

fn ident(token: Option<&Token>) -> Option<&str> {
    match token {
        Some(Token::Ident(name)) => Some(name),
        _ => None,
    }
}

fn analyse(tokens: &[Token], block: bool, out: &mut Vec<(Kind, String)>) {
    if block
        && ident(tokens.first()) == Some("filter")
        && let Some(name) = ident(tokens.get(1))
    {
        out.push((Kind::Filter, name.to_owned()));
    }
    for (i, token) in tokens.iter().enumerate() {
        let after_dot = i > 0 && tokens[i - 1] == Token::Dot;
        match token {
            Token::Pipe => {
                if let Some(name) = ident(tokens.get(i + 1)) {
                    out.push((Kind::Filter, name.to_owned()));
                }
            }
            Token::Ident(word) if word == "is" && !after_dot => {
                let mut next = i + 1;
                if ident(tokens.get(next)) == Some("not") && ident(tokens.get(next + 1)).is_some() {
                    next += 1;
                }
                if let Some(name) = ident(tokens.get(next)) {
                    out.push((Kind::Test, name.to_owned()));
                }
            }
            Token::Ident(name) if after_dot && tokens.get(i + 1) == Some(&Token::LParen) => {
                let on_loop = i >= 2
                    && ident(tokens.get(i - 2)) == Some("loop")
                    && !(i >= 3 && tokens[i - 3] == Token::Dot);
                if !(on_loop && LOOP_METHODS.contains(&name.as_str())) {
                    out.push((Kind::Method, name.clone()));
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::environment;

    fn names(source: &str) -> Vec<(Kind, String)> {
        scan(source)
    }

    fn f(name: &str) -> (Kind, String) {
        (Kind::Filter, name.to_owned())
    }

    #[test]
    fn finds_filters_tests_and_methods_in_every_tag() {
        assert_eq!(
            names("{{ a | upper | trim }}{% if b is not defined %}{% endif %}{{ c.title() }}"),
            [
                f("upper"),
                f("trim"),
                (Kind::Test, "defined".to_owned()),
                (Kind::Method, "title".to_owned()),
            ]
        );
        assert_eq!(names("{% filter upper %}x{% endfilter %}"), [f("upper")]);
        assert_eq!(names("{{ x is divisibleby(3) }}")[0].0, Kind::Test);
    }

    #[test]
    fn strings_comments_and_raw_bodies_are_skipped() {
        for source in [
            r#"{{ "a | nope" }} {{ 'it\'s | nope' }}"#,
            "{# {{ a | nope }} #}",
            "{% raw %}{{ a | nope }}{% endraw %}",
            "{%- raw -%}{{ a | nope }}{%- endraw -%}",
            "text | nope and x is nope and a.b(c) outside any tag",
            "{{ 1.5 }} {{ {'a': 1} }}",
        ] {
            assert_eq!(names(source), [], "{source}");
        }
        // Scanning resumes after the skipped span.
        assert_eq!(names("{# x #}{{ a | b }}"), [f("b")]);
        assert_eq!(
            names("{% raw %}{{ a | n }}{% endraw %}{{ a | b }}"),
            [f("b")]
        );
    }

    #[test]
    fn loop_methods_are_allowed_other_methods_are_not() {
        assert_eq!(names("{{ loop.cycle('a', 'b') }}{{ loop.changed(x) }}"), []);
        assert_eq!(
            names("{{ x.loop.cycle('a') }}"),
            [(Kind::Method, "cycle".to_owned())]
        );
    }

    #[test]
    fn only_names_the_sandbox_lacks_are_unknown() {
        let env = environment();
        let unknown = |source: &str| unknown_callables(&env, source);
        assert_eq!(
            unknown("{{ a | upper | length }}{% if a is string %}{% endif %}"),
            []
        );
        assert_eq!(
            unknown("{% if a %}{{ a | nosuchfilter }}{% elif b is nosuchtest %}{% endif %}"),
            [
                (Kind::Filter, "nosuchfilter".to_owned()),
                (Kind::Test, "nosuchtest".to_owned()),
            ]
        );
        assert_eq!(
            unknown("{{ a.upper() }}"),
            [(Kind::Method, "upper".to_owned())]
        );
    }
}
