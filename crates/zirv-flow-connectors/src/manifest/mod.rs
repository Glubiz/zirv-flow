//! The HTTP manifest (architecture 7.2): a JSON description of an API (auth, pagination, the
//! incremental cursor, field mapping, the delete signal) that the `http` source interprets. It
//! is data, never code: no expression language, no scripting.
//!
//! [`validate`] is pure (no clock, no I/O, no name resolution) and returns codes and JSON
//! pointers only, so an issue can be shown to anyone and logged: it never holds a value of the
//! manifest. The API's validate endpoint, the editor, the source's own `read` and `check`, and
//! the assistant's manifest tool all call it.

mod destination;
#[cfg(any(test, feature = "test-support"))]
mod fuzz;
mod issue;
mod model;
mod parse;
mod profile;

use serde_json::Value;

pub use destination::{
    BodyMatch, BounceKindSpec, DomainStatusSpec, ErrorAction, ErrorRule, FeedbackSpec,
    MessageField, Provider, SendMethod, SendSpec, StatusRange, VerificationSpec, place,
};
#[cfg(any(test, feature = "test-support"))]
pub use fuzz::check_manifest;
pub use issue::{Issue, code};
pub use model::{
    Auth, CredentialSpec, CursorType, DeleteSignal, FieldSpec, IncrementalSpec, KeyLocation,
    Manifest, PaginationSpec, RateLimitSpec, StreamSpec,
};
pub use profile::{
    KEY_PLACEHOLDER, KeySource, MAX_PROFILE_TRAITS, ProfileMethod, ProfileSpec, TraitPlacement,
};

use issue::{child, index};

/// The most bytes of a manifest, serialized.
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;
/// The deepest nesting of arrays and objects.
pub const MAX_DEPTH: usize = 12;

/// Checks `value` as manifest v1. The size, depth, stream and field limits are checked first and
/// end the check on their own: a manifest over a limit is not walked.
pub fn validate(value: &Value) -> Result<Manifest, Vec<Issue>> {
    let limits = check_limits(value);
    if !limits.is_empty() {
        return Err(limits);
    }
    let mut issues = Vec::new();
    scan_values(value, "", &mut issues);
    match parse::parse(value) {
        Ok(manifest) if issues.is_empty() => Ok(manifest),
        Ok(_) => Err(issues),
        Err(found) => {
            issues.extend(found);
            issues.truncate(issue::MAX_ISSUES);
            Err(issues)
        }
    }
}

/// Whether the manifest's auth is the authorization-code grant, which needs a person's consent
/// (the **Connect account** step). Looks only at `auth.type`, so it works on a draft.
pub fn needs_authorization(value: &Value) -> bool {
    value
        .get("auth")
        .and_then(|auth| auth.get("type"))
        .and_then(Value::as_str)
        == Some("oauth2_authorization_code")
}

/// The credential fields a manifest reads, which the source must hold. A source stores them as
/// its credential (names are fixed: a manifest carries no secret). `client_secret` of the
/// authorization-code grant is optional, as a public client has none: see
/// [`optional_credential_fields`].
pub fn declared_credential_fields(manifest: &Manifest) -> &'static [&'static str] {
    credential_fields_of(&manifest.auth)
}

/// [`declared_credential_fields`] for an auth section.
pub(crate) fn credential_fields_of(auth: &Auth) -> &'static [&'static str] {
    match auth {
        Auth::None => &[],
        Auth::ApiKey { .. } => &["api_key"],
        Auth::Basic => &["username", "password"],
        Auth::Bearer => &["token"],
        Auth::OAuth2ClientCredentials { .. } | Auth::OAuth2AuthorizationCode { .. } => {
            &["client_id", "client_secret"]
        }
    }
}

/// The subset of [`declared_credential_fields`] that may be absent.
pub fn optional_credential_fields(manifest: &Manifest) -> &'static [&'static str] {
    match manifest.auth {
        Auth::OAuth2AuthorizationCode { .. } => &["client_secret"],
        _ => &[],
    }
}

/// Size, depth, stream count and field count: what is checked before the walk.
fn check_limits(value: &Value) -> Vec<Issue> {
    if depth_exceeds(value, MAX_DEPTH) {
        return vec![Issue {
            code: code::TOO_DEEP,
            path: String::new(),
        }];
    }
    if serialized_size_exceeds(value, MAX_MANIFEST_BYTES) {
        return vec![Issue {
            code: code::MANIFEST_TOO_LARGE,
            path: String::new(),
        }];
    }
    let mut issues = Vec::new();
    if let Some(streams) = value.get("streams").and_then(Value::as_array) {
        if streams.len() > parse::MAX_STREAMS {
            issues.push(Issue {
                code: code::TOO_MANY_STREAMS,
                path: child("", "streams"),
            });
        }
        for (position, stream) in streams.iter().enumerate() {
            let too_many = stream
                .get("fields")
                .and_then(Value::as_array)
                .is_some_and(|fields| fields.len() > parse::MAX_FIELDS);
            if too_many && issues.len() < issue::MAX_ISSUES {
                issues.push(Issue {
                    code: code::TOO_MANY_FIELDS,
                    path: child(&index(&child("", "streams"), position), "fields"),
                });
            }
        }
    }
    issues
}

/// Whether `value` nests deeper than `limit` levels; an iterative walk.
fn depth_exceeds(value: &Value, limit: usize) -> bool {
    let mut stack = vec![(value, 1_usize)];
    while let Some((value, depth)) = stack.pop() {
        let children: Vec<&Value> = match value {
            Value::Array(items) => items.iter().collect(),
            Value::Object(map) => map.values().collect(),
            _ => continue,
        };
        if depth > limit {
            return true;
        }
        stack.extend(children.into_iter().map(|child| (child, depth + 1)));
    }
    false
}

/// Whether the JSON text of `value` is longer than `limit` bytes, stopping at the limit.
fn serialized_size_exceeds(value: &Value, limit: usize) -> bool {
    struct Counter {
        written: usize,
        limit: usize,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written += buf.len();
            if self.written > self.limit {
                return Err(std::io::Error::other("over the limit"));
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { written: 0, limit };
    serde_json::to_writer(&mut counter, value).is_err()
}

/// Reports a string value holding a template marker anywhere in the manifest.
fn scan_values(value: &Value, path: &str, issues: &mut Vec<Issue>) {
    if issues.len() >= issue::MAX_ISSUES {
        return;
    }
    match value {
        Value::String(text) if parse::has_template_marker(text) => issues.push(Issue {
            code: code::EXPRESSION_NOT_SUPPORTED,
            path: path.to_owned(),
        }),
        Value::Array(items) => {
            for (position, item) in items.iter().enumerate() {
                let at = index(path, position);
                scan_values(item, &at, issues);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                let at = child(path, key);
                scan_values(item, &at, issues);
            }
        }
        _ => {}
    }
}
