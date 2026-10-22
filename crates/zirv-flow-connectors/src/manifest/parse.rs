//! The validator: a hand-written walk over a `serde_json::Value` that consumes the keys it
//! knows through a small cursor type, reports the leftovers as unknown, and collects issues.
//! Nothing is copied from a value into an issue, because `serde`'s errors quote names and
//! values. Pure: no clock, no I/O, no name resolution.

use hashbrown::HashSet;
use std::collections::BTreeSet;

use serde_json::{Map, Value};
use url::Url;
use zirv_flow_core::credentials::{CredentialKind, CredentialSchema};
use zirv_flow_core::mapping::{Header, Mapping, Target, TraitCatalog, TraitState, validate_paths};
use zirv_flow_core::people::TraitType;

use super::issue::{Issue, MAX_ISSUES, child, code, index};
use super::model::{
    Auth, CredentialSpec, CursorType, DeleteSignal, FieldSpec, IncrementalSpec, KeyLocation,
    Manifest, PaginationSpec, RateLimitSpec, StreamSpec,
};
use crate::runtime::ClientAuth;
use crate::sources::{FieldType, url_problem_https};

pub(super) const MAX_STREAMS: usize = 20;
pub(super) const MAX_FIELDS: usize = 200;
const MAX_HEADERS: usize = 10;
const MAX_QUERY: usize = 20;
const MAX_NAME: usize = 256;
const MAX_PATH: usize = 512;
const MAX_TEXT: usize = 256;
const MAX_QUERY_VALUE: usize = 1024;
const MAX_SCOPES: usize = 20;

/// Keys that suggest a script. They get a pointed code instead of `unknown_field`.
pub(super) const SCRIPT_KEYS: &[&str] = &[
    "script",
    "expression",
    "eval",
    "js",
    "code",
    "template",
    "transform_fn",
    "lambda",
];

/// Headers the runtime or the transport owns; a manifest cannot set them.
const FORBIDDEN_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "range",
];

const FIELD_TYPES: &[(&str, FieldType)] = &[
    ("string", FieldType::String),
    ("number", FieldType::Number),
    ("boolean", FieldType::Boolean),
    ("timestamp", FieldType::Timestamp),
    ("json", FieldType::Json),
    ("unknown", FieldType::Unknown),
];

/// A trait catalog that knows every key: whether a trait exists is checked when the mapping is
/// saved, in a workspace (a manifest is workspace-free data).
struct AnyTrait;

impl TraitCatalog for AnyTrait {
    fn trait_state(&self, _key: &str) -> TraitState {
        TraitState::Active(TraitType::String)
    }
}

/// An object being read: the keys taken so far, so the rest can be reported.
struct Obj<'v> {
    map: &'v Map<String, Value>,
    path: String,
    taken: Vec<&'static str>,
}

impl<'v> Obj<'v> {
    fn at(&self, key: &str) -> String {
        child(&self.path, key)
    }

    /// The value of `key`; a null counts as absent.
    fn take(&mut self, key: &'static str) -> Option<&'v Value> {
        self.taken.push(key);
        self.map.get(key).filter(|value| !value.is_null())
    }
}

#[derive(Default)]
struct Walk {
    issues: Vec<Issue>,
}

impl Walk {
    fn add(&mut self, code: &'static str, path: String) {
        if self.issues.len() < MAX_ISSUES {
            self.issues.push(Issue { code, path });
        }
    }

    fn object<'v>(&mut self, value: &'v Value, path: String) -> Option<Obj<'v>> {
        match value.as_object() {
            Some(map) => Some(Obj {
                map,
                path,
                taken: Vec::new(),
            }),
            None => {
                self.add(code::INVALID_TYPE, path);
                None
            }
        }
    }

    /// Reports every key the walk did not take.
    fn finish(&mut self, obj: Obj<'_>) {
        for key in obj.map.keys() {
            if obj.taken.contains(&key.as_str()) {
                continue;
            }
            let issue = if SCRIPT_KEYS.contains(&key.as_str()) {
                code::EXPRESSION_NOT_SUPPORTED
            } else {
                code::UNKNOWN_FIELD
            };
            self.add(issue, obj.at(key));
        }
    }

    fn missing(&mut self, obj: &Obj<'_>, key: &str) {
        self.add(code::REQUIRED, obj.at(key));
    }

    /// A text of `min..=max` characters.
    fn text<'v>(
        &mut self,
        obj: &mut Obj<'v>,
        key: &'static str,
        required: bool,
        max: usize,
    ) -> Option<&'v str> {
        let Some(value) = obj.take(key) else {
            if required {
                self.missing(obj, key);
            }
            return None;
        };
        let Some(text) = value.as_str() else {
            self.add(code::INVALID_TYPE, obj.at(key));
            return None;
        };
        if text.trim().is_empty() || text.len() > max {
            self.add(code::INVALID_LENGTH, obj.at(key));
            return None;
        }
        Some(text)
    }

    fn number(
        &mut self,
        obj: &mut Obj<'_>,
        key: &'static str,
        required: bool,
        range: std::ops::RangeInclusive<u32>,
    ) -> Option<u32> {
        let Some(value) = obj.take(key) else {
            if required {
                self.missing(obj, key);
            }
            return None;
        };
        let Some(number) = value.as_u64() else {
            self.add(
                if value.is_number() {
                    code::OUT_OF_RANGE
                } else {
                    code::INVALID_TYPE
                },
                obj.at(key),
            );
            return None;
        };
        match u32::try_from(number) {
            Ok(number) if range.contains(&number) => Some(number),
            _ => {
                self.add(code::OUT_OF_RANGE, obj.at(key));
                None
            }
        }
    }

    /// A text that is one of `choices`.
    fn choice<'v>(
        &mut self,
        obj: &mut Obj<'v>,
        key: &'static str,
        required: bool,
        choices: &[&str],
    ) -> Option<&'v str> {
        let Some(value) = obj.take(key) else {
            if required {
                self.missing(obj, key);
            }
            return None;
        };
        match value.as_str() {
            Some(text) if choices.contains(&text) => Some(text),
            Some(_) => {
                self.add(code::INVALID_VALUE, obj.at(key));
                None
            }
            None => {
                self.add(code::INVALID_TYPE, obj.at(key));
                None
            }
        }
    }

    fn url(&mut self, obj: &mut Obj<'_>, key: &'static str, allow_query: bool) -> Option<Url> {
        let Some(value) = obj.take(key) else {
            self.missing(obj, key);
            return None;
        };
        let Some(text) = value.as_str() else {
            self.add(code::INVALID_TYPE, obj.at(key));
            return None;
        };
        match url_problem_https(text) {
            None => match Url::parse(text) {
                Ok(url) if allow_query || url.query().is_none() => Some(url),
                _ => {
                    self.add(code::INVALID_URL, obj.at(key));
                    None
                }
            },
            Some(_) => {
                self.add(code::INVALID_URL, obj.at(key));
                None
            }
        }
    }

    fn pointer<'v>(
        &mut self,
        obj: &mut Obj<'v>,
        key: &'static str,
        required: bool,
    ) -> Option<&'v str> {
        let Some(value) = obj.take(key) else {
            if required {
                self.missing(obj, key);
            }
            return None;
        };
        match value.as_str() {
            Some(text) if text.len() <= MAX_PATH && (text.is_empty() || text.starts_with('/')) => {
                Some(text)
            }
            Some(_) => {
                self.add(code::INVALID_POINTER, obj.at(key));
                None
            }
            None => {
                self.add(code::INVALID_TYPE, obj.at(key));
                None
            }
        }
    }

    fn scopes(&mut self, obj: &mut Obj<'_>) -> Vec<String> {
        let Some(value) = obj.take("scopes") else {
            return Vec::new();
        };
        let Some(items) = value.as_array() else {
            self.add(code::INVALID_TYPE, obj.at("scopes"));
            return Vec::new();
        };
        if items.len() > MAX_SCOPES {
            self.add(code::OUT_OF_RANGE, obj.at("scopes"));
            return Vec::new();
        }
        let mut scopes = Vec::new();
        for (position, item) in items.iter().enumerate() {
            match item.as_str() {
                Some(text)
                    if !text.is_empty()
                        && text.len() <= MAX_TEXT
                        && !text.chars().any(|ch| ch.is_whitespace() || ch.is_control()) =>
                {
                    scopes.push(text.to_owned());
                }
                Some(_) => self.add(code::INVALID_VALUE, index(&obj.at("scopes"), position)),
                None => self.add(code::INVALID_TYPE, index(&obj.at("scopes"), position)),
            }
        }
        scopes
    }

    fn client_auth(&mut self, obj: &mut Obj<'_>) -> ClientAuth {
        match self.choice(obj, "client_auth", false, &["body", "basic"]) {
            Some("basic") => ClientAuth::Basic,
            _ => ClientAuth::Body,
        }
    }
}

/// Walks a manifest that passed the size checks.
pub(super) fn parse(value: &Value) -> Result<Manifest, Vec<Issue>> {
    let mut walk = Walk::default();
    let manifest = manifest(&mut walk, value);
    match manifest {
        Some(manifest) if walk.issues.is_empty() => Ok(manifest),
        _ => Err(walk.issues),
    }
}

fn manifest(walk: &mut Walk, value: &Value) -> Option<Manifest> {
    let mut obj = walk.object(value, String::new())?;
    match obj.take("version") {
        None => walk.missing(&obj, "version"),
        Some(version) if version.as_str() == Some("v1") => {}
        Some(version) if version.is_string() => {
            walk.add(code::UNSUPPORTED_VERSION, obj.at("version"));
        }
        Some(_) => walk.add(code::INVALID_TYPE, obj.at("version")),
    }
    let base_url = base_url(walk, &mut obj);
    let (auth, credential) = match obj.take("auth") {
        Some(value) => auth(walk, value, obj.at("auth")),
        None => {
            walk.missing(&obj, "auth");
            (None, None)
        }
    };
    let rate_limit = obj
        .take("rate_limit")
        .and_then(|value| rate_limit(walk, value, obj.at("rate_limit")));
    let headers = obj
        .take("headers")
        .map(|value| headers(walk, value, obj.at("headers")))
        .unwrap_or_default();
    let streams = match obj.take("streams") {
        Some(value) => streams(walk, value, obj.at("streams")),
        None => {
            walk.missing(&obj, "streams");
            Vec::new()
        }
    };
    walk.finish(obj);
    Some(Manifest {
        base_url: base_url?,
        auth: auth?,
        credential,
        rate_limit,
        headers,
        streams,
    })
}

fn base_url(walk: &mut Walk, obj: &mut Obj<'_>) -> Option<Url> {
    walk.url(obj, "base_url", false)
}

fn auth(walk: &mut Walk, value: &Value, path: String) -> (Option<Auth>, Option<CredentialSpec>) {
    let Some(mut obj) = walk.object(value, path) else {
        return (None, None);
    };
    let kind = walk.choice(
        &mut obj,
        "type",
        true,
        &[
            "none",
            "api_key",
            "basic",
            "bearer",
            "oauth2_client_credentials",
            "oauth2_authorization_code",
        ],
    );
    let Some(kind) = kind else {
        return (None, None);
    };
    let auth_path = obj.path.clone();
    let credential = obj
        .take("credential")
        .and_then(|value| credential_spec(walk, value, obj.at("credential")));
    let auth = match kind {
        "none" => Some(Auth::None),
        "basic" => Some(Auth::Basic),
        "bearer" => Some(Auth::Bearer),
        "api_key" => {
            let location = walk.choice(&mut obj, "in", true, &["header", "query"]);
            let name = walk.text(&mut obj, "name", true, MAX_NAME);
            match (location, name) {
                (Some(location), Some(name)) => {
                    let location = if location == "header" {
                        KeyLocation::Header
                    } else {
                        KeyLocation::Query
                    };
                    // The key itself is the credential, so it may be the `Authorization` header.
                    if location == KeyLocation::Header
                        && !name.eq_ignore_ascii_case("authorization")
                        && is_forbidden_header(name)
                    {
                        walk.add(code::FORBIDDEN_HEADER, obj.at("name"));
                        None
                    } else if location == KeyLocation::Header && !is_token(name) {
                        walk.add(code::INVALID_HEADER, obj.at("name"));
                        None
                    } else {
                        Some(Auth::ApiKey {
                            location,
                            name: name.to_owned(),
                        })
                    }
                }
                _ => None,
            }
        }
        "oauth2_client_credentials" => {
            let token_url = walk.url(&mut obj, "token_url", true);
            let scopes = walk.scopes(&mut obj);
            let client_auth = walk.client_auth(&mut obj);
            token_url.map(|token_url| Auth::OAuth2ClientCredentials {
                token_url,
                scopes,
                client_auth,
            })
        }
        _ => {
            let authorize_url = walk.url(&mut obj, "authorize_url", true);
            let token_url = walk.url(&mut obj, "token_url", true);
            let scopes = walk.scopes(&mut obj);
            let client_auth = walk.client_auth(&mut obj);
            match (authorize_url, token_url) {
                (Some(authorize_url), Some(token_url)) => Some(Auth::OAuth2AuthorizationCode {
                    authorize_url,
                    token_url,
                    scopes,
                    client_auth,
                }),
                _ => None,
            }
        }
    };
    walk.finish(obj);
    if let (Some(auth), Some(spec)) = (&auth, &credential) {
        let schema = spec.schema();
        let optional: &[&str] = match auth {
            Auth::OAuth2AuthorizationCode { .. } => &["client_secret"],
            _ => &[],
        };
        let reads = super::credential_fields_of(auth);
        if reads
            .iter()
            .any(|name| !optional.contains(name) && schema.field(name).is_none())
        {
            walk.add(
                code::CREDENTIAL_FIELD_MISSING,
                child(&auth_path, "credential"),
            );
        }
    }
    (auth, credential)
}

/// `auth.credential`: `{"kind": "<built-in kind>"}` or `{"fields": [...]}`, one of the two.
fn credential_spec(walk: &mut Walk, value: &Value, path: String) -> Option<CredentialSpec> {
    let mut obj = walk.object(value, path)?;
    let kind = obj.take("kind");
    let fields = obj.take("fields");
    let spec = match (kind, fields) {
        (Some(Value::String(name)), None) => match CredentialKind::parse(name) {
            Some(kind) => Some(CredentialSpec::Kind(kind)),
            None => {
                walk.add(code::INVALID_VALUE, obj.at("kind"));
                None
            }
        },
        (Some(_), None) => {
            walk.add(code::INVALID_TYPE, obj.at("kind"));
            None
        }
        (None, Some(fields)) => {
            let parsed: Result<Vec<_>, _> = fields
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .map(|item| serde_json::from_value(item.clone()))
                        .collect()
                })
                .unwrap_or_else(|| Ok(Vec::new()));
            match (fields.is_array(), parsed) {
                (true, Ok(fields)) => {
                    let schema = CredentialSchema { fields };
                    if schema.check().is_ok() {
                        Some(CredentialSpec::Schema(schema))
                    } else {
                        walk.add(code::INVALID_CREDENTIAL_SCHEMA, obj.at("fields"));
                        None
                    }
                }
                (false, _) => {
                    walk.add(code::INVALID_TYPE, obj.at("fields"));
                    None
                }
                (true, Err(_)) => {
                    walk.add(code::INVALID_CREDENTIAL_SCHEMA, obj.at("fields"));
                    None
                }
            }
        }
        (Some(_), Some(_)) => {
            walk.add(code::INVALID_VALUE, obj.at("fields"));
            None
        }
        (None, None) => {
            walk.missing(&obj, "kind");
            None
        }
    };
    walk.finish(obj);
    spec
}

fn rate_limit(walk: &mut Walk, value: &Value, path: String) -> Option<RateLimitSpec> {
    let mut obj = walk.object(value, path)?;
    let requests = walk.number(&mut obj, "requests", true, 1..=1000);
    let per_seconds = walk.number(&mut obj, "per_seconds", true, 1..=3600);
    walk.finish(obj);
    Some(RateLimitSpec {
        requests: requests?,
        per_seconds: per_seconds?,
    })
}

/// A header or parameter name: the characters of an RFC 9110 token.
fn is_token(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

/// Whether a manifest may not send a header of this name: the runtime or the transport owns it, or it
/// makes the request conditional.
fn is_forbidden_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    FORBIDDEN_HEADERS.contains(&lower.as_str()) || lower.starts_with("if-")
}

fn headers(walk: &mut Walk, value: &Value, path: String) -> Vec<(String, String)> {
    let Some(map) = value.as_object() else {
        walk.add(code::INVALID_TYPE, path);
        return Vec::new();
    };
    if map.len() > MAX_HEADERS {
        walk.add(code::TOO_MANY_HEADERS, path);
        return Vec::new();
    }
    let mut headers = Vec::new();
    for (name, value) in map {
        let at = child(&path, name);
        if is_forbidden_header(name) {
            walk.add(code::FORBIDDEN_HEADER, at);
            continue;
        }
        if !is_token(name) {
            walk.add(code::INVALID_HEADER, at);
            continue;
        }
        match value.as_str() {
            Some(text)
                if text.len() <= MAX_TEXT
                    && !text.chars().any(|ch| ch.is_control() && ch != '\t') =>
            {
                headers.push((name.clone(), text.to_owned()));
            }
            Some(_) => walk.add(code::INVALID_HEADER, at),
            None => walk.add(code::INVALID_TYPE, at),
        }
    }
    headers
}

fn streams(walk: &mut Walk, value: &Value, path: String) -> Vec<StreamSpec> {
    let Some(items) = value.as_array() else {
        walk.add(code::INVALID_TYPE, path);
        return Vec::new();
    };
    if items.is_empty() {
        walk.add(code::OUT_OF_RANGE, path);
        return Vec::new();
    }
    let mut names = HashSet::new();
    let mut streams = Vec::new();
    for (position, item) in items.iter().enumerate() {
        let at = index(&path, position);
        if let Some(stream) = stream(walk, item, at.clone()) {
            if !names.insert(stream.name.clone()) {
                walk.add(code::DUPLICATE_STREAM, child(&at, "name"));
            }
            streams.push(stream);
        }
    }
    streams
}

fn stream(walk: &mut Walk, value: &Value, path: String) -> Option<StreamSpec> {
    let mut obj = walk.object(value, path)?;
    let name = walk.text(&mut obj, "name", true, MAX_NAME);
    let stream_path = stream_path(walk, &mut obj);
    let query = obj
        .take("query")
        .map(|value| query(walk, value, obj.at("query")))
        .unwrap_or_default();
    let records_path = walk.pointer(&mut obj, "records_path", false);
    let pagination = match obj.take("pagination") {
        Some(value) => pagination(walk, value, obj.at("pagination")),
        None => Some(PaginationSpec::None),
    };
    let fields = match obj.take("fields") {
        Some(value) => fields(walk, value, obj.at("fields")),
        None => {
            walk.missing(&obj, "fields");
            Vec::new()
        }
    };
    let primary_key = primary_key(walk, &mut obj, &fields);
    let incremental = obj
        .take("incremental")
        .and_then(|value| incremental(walk, value, obj.at("incremental"), &fields));
    let delete_signal = obj
        .take("delete_signal")
        .and_then(|value| delete_signal(walk, value, obj.at("delete_signal")));
    let mapping = obj
        .take("mapping")
        .and_then(|value| mapping(walk, value, obj.at("mapping"), &fields));
    walk.finish(obj);
    Some(StreamSpec {
        name: name?.to_owned(),
        path: stream_path?,
        query,
        records_path: records_path.unwrap_or_default().to_owned(),
        pagination: pagination?,
        fields,
        primary_key,
        incremental,
        delete_signal,
        mapping,
    })
}

fn stream_path(walk: &mut Walk, obj: &mut Obj<'_>) -> Option<String> {
    let value = obj.take("path");
    let Some(value) = value else {
        walk.missing(obj, "path");
        return None;
    };
    let Some(text) = value.as_str() else {
        walk.add(code::INVALID_TYPE, obj.at("path"));
        return None;
    };
    let ok = text.starts_with('/')
        && text.len() <= MAX_PATH
        && !text.contains("//")
        && !text.contains("..")
        && !text.contains(['?', '#', '\\', '%'])
        && !text.chars().any(char::is_control);
    if !ok {
        walk.add(code::PATH_INVALID, obj.at("path"));
        return None;
    }
    Some(text.to_owned())
}

fn query(walk: &mut Walk, value: &Value, path: String) -> Vec<(String, String)> {
    let Some(map) = value.as_object() else {
        walk.add(code::INVALID_TYPE, path);
        return Vec::new();
    };
    if map.len() > MAX_QUERY {
        walk.add(code::TOO_MANY_QUERY, path);
        return Vec::new();
    }
    let mut query = Vec::new();
    for (name, value) in map {
        let at = child(&path, name);
        if name.trim().is_empty() || name.len() > MAX_NAME {
            walk.add(code::INVALID_LENGTH, at);
            continue;
        }
        match value {
            Value::String(text) if text.len() <= MAX_QUERY_VALUE => {
                query.push((name.clone(), text.clone()));
            }
            Value::String(_) => walk.add(code::INVALID_LENGTH, at),
            Value::Number(number) => query.push((name.clone(), number.to_string())),
            Value::Bool(flag) => query.push((name.clone(), flag.to_string())),
            _ => walk.add(code::INVALID_TYPE, at),
        }
    }
    query
}

fn pagination(walk: &mut Walk, value: &Value, path: String) -> Option<PaginationSpec> {
    let mut obj = walk.object(value, path)?;
    let kind = walk.choice(
        &mut obj,
        "type",
        true,
        &["none", "cursor", "offset", "page", "link_header"],
    )?;
    let spec = match kind {
        "none" => Some(PaginationSpec::None),
        "link_header" => Some(PaginationSpec::LinkHeader),
        "cursor" => {
            let param = walk.text(&mut obj, "param", true, MAX_NAME);
            let next = walk.pointer(&mut obj, "next", true);
            match (param, next) {
                (Some(param), Some(next)) if !next.is_empty() => Some(PaginationSpec::Cursor {
                    param: param.to_owned(),
                    next: next.to_owned(),
                }),
                (Some(_), Some(_)) => {
                    walk.add(code::INVALID_POINTER, obj.at("next"));
                    None
                }
                _ => None,
            }
        }
        "offset" => {
            let param = walk.text(&mut obj, "param", true, MAX_NAME);
            let limit_param = walk.text(&mut obj, "limit_param", true, MAX_NAME);
            let limit = walk.number(&mut obj, "limit", true, 1..=1000);
            match (param, limit_param, limit) {
                (Some(param), Some(limit_param), Some(limit)) => Some(PaginationSpec::Offset {
                    param: param.to_owned(),
                    limit_param: limit_param.to_owned(),
                    limit,
                }),
                _ => None,
            }
        }
        _ => {
            let param = walk.text(&mut obj, "param", true, MAX_NAME);
            let first = walk.number(&mut obj, "first", false, 0..=1_000_000);
            let size_param = walk.text(&mut obj, "size_param", false, MAX_NAME);
            let size = walk.number(&mut obj, "size", false, 1..=1000);
            let sized = match (size_param, size) {
                (Some(name), Some(size)) => Some((name.to_owned(), size)),
                (None, None) => None,
                (Some(_), None) => {
                    walk.missing(&obj, "size");
                    None
                }
                (None, Some(_)) => {
                    walk.missing(&obj, "size_param");
                    None
                }
            };
            param.map(|param| PaginationSpec::Page {
                param: param.to_owned(),
                first: first.unwrap_or(1),
                size: sized,
            })
        }
    };
    walk.finish(obj);
    spec
}

fn fields(walk: &mut Walk, value: &Value, path: String) -> Vec<FieldSpec> {
    let Some(items) = value.as_array() else {
        walk.add(code::INVALID_TYPE, path);
        return Vec::new();
    };
    if items.is_empty() {
        walk.add(code::OUT_OF_RANGE, path);
        return Vec::new();
    }
    let mut names = HashSet::new();
    let mut fields = Vec::new();
    for (position, item) in items.iter().enumerate() {
        let Some(mut obj) = walk.object(item, index(&path, position)) else {
            continue;
        };
        let name = match obj.take("name").map(Value::as_str) {
            None => {
                walk.missing(&obj, "name");
                None
            }
            Some(Some(text)) if !text.trim().is_empty() && text.len() <= MAX_NAME => Some(text),
            Some(Some(_)) => {
                walk.add(code::INVALID_FIELD_NAME, obj.at("name"));
                None
            }
            Some(None) => {
                walk.add(code::INVALID_TYPE, obj.at("name"));
                None
            }
        };
        let names_of: Vec<&str> = FIELD_TYPES.iter().map(|(name, _)| *name).collect();
        let field_type = walk
            .choice(&mut obj, "type", true, &names_of)
            .and_then(|chosen| {
                FIELD_TYPES
                    .iter()
                    .find(|(name, _)| *name == chosen)
                    .map(|(_, field_type)| *field_type)
            });
        walk.finish(obj);
        if let (Some(name), Some(field_type)) = (name, field_type) {
            if names.insert(name) {
                fields.push(FieldSpec {
                    name: name.to_owned(),
                    field_type,
                });
            } else {
                walk.add(
                    code::DUPLICATE_FIELD,
                    child(&index(&path, position), "name"),
                );
            }
        }
    }
    fields
}

fn primary_key(walk: &mut Walk, obj: &mut Obj<'_>, fields: &[FieldSpec]) -> Vec<String> {
    let Some(value) = obj.take("primary_key") else {
        walk.missing(obj, "primary_key");
        return Vec::new();
    };
    let at = obj.at("primary_key");
    let Some(items) = value.as_array() else {
        walk.add(code::INVALID_TYPE, at);
        return Vec::new();
    };
    if items.is_empty() {
        walk.add(code::PRIMARY_KEY_EMPTY, at);
        return Vec::new();
    }
    let known: BTreeSet<&str> = fields.iter().map(|field| field.name.as_str()).collect();
    let mut key = Vec::new();
    for (position, item) in items.iter().enumerate() {
        match item.as_str() {
            Some(name) if known.contains(name) => key.push(name.to_owned()),
            Some(_) => walk.add(code::PRIMARY_KEY_UNKNOWN_FIELD, index(&at, position)),
            None => walk.add(code::INVALID_TYPE, index(&at, position)),
        }
    }
    key
}

fn incremental(
    walk: &mut Walk,
    value: &Value,
    path: String,
    fields: &[FieldSpec],
) -> Option<IncrementalSpec> {
    let mut obj = walk.object(value, path)?;
    let cursor_field = walk.text(&mut obj, "cursor_field", true, MAX_NAME);
    let request_param = walk.text(&mut obj, "request_param", true, MAX_NAME);
    let cursor_type = walk.choice(&mut obj, "cursor_type", true, &["timestamp", "integer"]);
    if let Some(field) = cursor_field
        && !fields.iter().any(|known| known.name == field)
    {
        walk.add(code::CURSOR_FIELD_UNKNOWN, obj.at("cursor_field"));
    }
    walk.finish(obj);
    Some(IncrementalSpec {
        cursor_field: cursor_field?.to_owned(),
        request_param: request_param?.to_owned(),
        cursor_type: match cursor_type? {
            "timestamp" => CursorType::Timestamp,
            _ => CursorType::Integer,
        },
    })
}

fn delete_signal(walk: &mut Walk, value: &Value, path: String) -> Option<DeleteSignal> {
    let mut obj = walk.object(value, path)?;
    let signal_path = walk.text(&mut obj, "path", true, MAX_NAME);
    let equals = match obj.take("equals") {
        None => {
            walk.missing(&obj, "equals");
            None
        }
        Some(Value::String(text)) if text.len() <= MAX_TEXT => Some(Value::String(text.clone())),
        Some(Value::String(_)) => {
            walk.add(code::INVALID_LENGTH, obj.at("equals"));
            None
        }
        Some(value @ (Value::Number(_) | Value::Bool(_))) => Some(value.clone()),
        Some(_) => {
            walk.add(code::INVALID_TYPE, obj.at("equals"));
            None
        }
    };
    walk.finish(obj);
    Some(DeleteSignal {
        path: signal_path?.to_owned(),
        equals: equals?,
    })
}

/// Keys the `mapping` section may hold, by the role of the object. A key outside is reported
/// like any other unknown key; a value is never looked at, and the keys of a `value_map` are
/// data, not structure.
fn mapping_keys(walk: &mut Walk, value: &Value, path: &str) {
    fn keys(walk: &mut Walk, value: &Value, path: &str, allowed: &[&str]) {
        let Some(map) = value.as_object() else {
            return;
        };
        for key in map.keys() {
            if allowed.contains(&key.as_str()) {
                continue;
            }
            let issue = if SCRIPT_KEYS.contains(&key.as_str()) {
                code::EXPRESSION_NOT_SUPPORTED
            } else {
                code::UNKNOWN_FIELD
            };
            walk.add(issue, child(path, key));
        }
    }
    fn each(walk: &mut Walk, value: Option<&Value>, path: &str, allowed: &[&str]) {
        if let Some(items) = value.and_then(Value::as_array) {
            for (position, item) in items.iter().enumerate() {
                keys(walk, item, &index(path, position), allowed);
            }
        }
    }
    const RULE: &[&str] = &["column", "target", "transforms"];
    const TRANSFORM: &[&str] = &["op", "to", "format", "columns", "separator", "value", "map"];

    keys(
        walk,
        value,
        path,
        &["key_column", "columns", "event", "delete_when"],
    );
    let Some(map) = value.as_object() else {
        return;
    };
    each(walk, map.get("columns"), &child(path, "columns"), RULE);
    if let Some(columns) = map.get("columns").and_then(Value::as_array) {
        for (position, rule) in columns.iter().enumerate() {
            let at = index(&child(path, "columns"), position);
            if let Some(target) = rule.get("target") {
                keys(
                    walk,
                    target,
                    &child(&at, "target"),
                    &["type", "kind", "key", "list_id"],
                );
            }
            each(
                walk,
                rule.get("transforms"),
                &child(&at, "transforms"),
                TRANSFORM,
            );
        }
    }
    if let Some(event) = map.get("event") {
        let at = child(path, "event");
        keys(walk, event, &at, &["name", "timestamp", "properties"]);
        if let Some(timestamp) = event.get("timestamp") {
            let at = child(&at, "timestamp");
            keys(walk, timestamp, &at, &["column", "transforms"]);
            each(
                walk,
                timestamp.get("transforms"),
                &child(&at, "transforms"),
                TRANSFORM,
            );
        }
        each(
            walk,
            event.get("properties"),
            &child(&at, "properties"),
            &["property", "column", "transforms"],
        );
    }
    if let Some(signal) = map.get("delete_when") {
        keys(
            walk,
            signal,
            &child(path, "delete_when"),
            &["column", "rule"],
        );
    }
}

fn mapping(walk: &mut Walk, value: &Value, path: String, fields: &[FieldSpec]) -> Option<Mapping> {
    if !value.is_object() {
        walk.add(code::INVALID_TYPE, path);
        return None;
    }
    let before = walk.issues.len();
    mapping_keys(walk, value, &path);
    if walk.issues.len() > before {
        return None;
    }
    let Ok(mapping) = serde_json::from_value::<Mapping>(value.clone()) else {
        walk.add(code::INVALID_MAPPING, path);
        return None;
    };
    for (position, rule) in mapping.columns.iter().enumerate() {
        if matches!(rule.target, Target::List { .. }) {
            walk.add(
                code::LIST_TARGET_NOT_ALLOWED,
                child(&index(&child(&path, "columns"), position), "target"),
            );
        }
    }
    if walk.issues.len() > before {
        return None;
    }
    // Field names are unique and non-blank already, so a header can be built.
    let Ok(header) = Header::new(fields.iter().map(|field| field.name.clone()).collect()) else {
        return None;
    };
    if let Err(error) = validate_paths(&mapping, &header, &AnyTrait) {
        walk.add(error.code(), path);
        return None;
    }
    Some(mapping)
}

/// Whether `text` holds a template marker.
pub(super) fn has_template_marker(text: &str) -> bool {
    ["{{", "${", "{%", "<%"]
        .iter()
        .any(|marker| text.contains(marker))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::{code, validate};
    use super::*;

    /// A small valid manifest to break one thing at a time.
    fn valid() -> Value {
        json!({
            "version": "v1",
            "base_url": "https://api.example.test/v1",
            "auth": {"type": "none"},
            "streams": [{
                "name": "contacts",
                "path": "/contacts",
                "fields": [
                    {"name": "id", "type": "string"},
                    {"name": "email", "type": "string"},
                    {"name": "updated_at", "type": "timestamp"}
                ],
                "primary_key": ["id"]
            }]
        })
    }

    fn found(value: &Value) -> Vec<(&'static str, String)> {
        match validate(value) {
            Ok(_) => Vec::new(),
            Err(issues) => issues
                .into_iter()
                .map(|issue| (issue.code, issue.path))
                .collect(),
        }
    }

    /// The manifest with `edit` applied to it.
    fn with(edit: impl FnOnce(&mut Value)) -> Value {
        let mut value = valid();
        edit(&mut value);
        value
    }

    fn assert_issue(value: &Value, code: &str, path: &str) {
        let issues = found(value);
        assert!(
            issues.iter().any(|(c, p)| *c == code && p == path),
            "expected {code} at {path}, got {issues:?}"
        );
    }

    fn assert_valid(value: &Value) {
        assert_eq!(found(value), Vec::new(), "{value}");
    }

    #[test]
    fn every_reference_manifest_is_valid() {
        let manifests = [
            include_str!("../../tests/fixtures/manifests/cursor_oauth2_code.json"),
            include_str!("../../tests/fixtures/manifests/offset_api_key.json"),
            include_str!("../../tests/fixtures/manifests/page_basic_soft_delete.json"),
            include_str!("../../tests/fixtures/manifests/link_header_bearer.json"),
        ];
        for text in manifests {
            let value: Value = serde_json::from_str(text).unwrap();
            let manifest = validate(&value).unwrap_or_else(|issues| panic!("{issues:?}"));
            assert!(!manifest.streams.is_empty());
        }
    }

    #[test]
    fn the_top_level_takes_a_version_a_base_url_and_streams() {
        assert_valid(&valid());
        assert_issue(
            &with(|v| {
                v.as_object_mut().unwrap().remove("version");
            }),
            code::REQUIRED,
            "/version",
        );
        assert_issue(
            &with(|v| v["version"] = json!("v2")),
            code::UNSUPPORTED_VERSION,
            "/version",
        );
        assert_issue(
            &with(|v| v["version"] = json!(1)),
            code::INVALID_TYPE,
            "/version",
        );
        for bad in ["ftp://api.example.test", "not a url", "https://u:p@a.test/"] {
            assert_issue(
                &with(|v| v["base_url"] = json!(bad)),
                code::INVALID_URL,
                "/base_url",
            );
        }
        assert_issue(
            &with(|v| v["streams"] = json!([])),
            code::OUT_OF_RANGE,
            "/streams",
        );
        assert_issue(&json!([]), code::INVALID_TYPE, "");
    }

    #[test]
    fn every_auth_type_is_accepted_with_its_parameters() {
        let auths = [
            json!({"type": "none"}),
            json!({"type": "api_key", "in": "query", "name": "key"}),
            json!({"type": "api_key", "in": "header", "name": "X-Key"}),
            json!({"type": "basic"}),
            json!({"type": "bearer"}),
            json!({"type": "oauth2_client_credentials",
                   "token_url": "https://a.test/token", "scopes": ["a", "b"],
                   "client_auth": "basic"}),
            json!({"type": "oauth2_authorization_code",
                   "authorize_url": "https://a.test/authorize",
                   "token_url": "https://a.test/token"}),
        ];
        for auth in auths {
            assert_valid(&with(|v| v["auth"] = auth.clone()));
        }
    }

    #[test]
    fn a_bad_auth_names_the_key() {
        assert_issue(
            &with(|v| v["auth"] = json!({"type": "magic"})),
            code::INVALID_VALUE,
            "/auth/type",
        );
        assert_issue(
            &with(|v| v["auth"] = json!({})),
            code::REQUIRED,
            "/auth/type",
        );
        assert_issue(
            &with(|v| v["auth"] = json!({"type": "api_key", "in": "header"})),
            code::REQUIRED,
            "/auth/name",
        );
        assert_issue(
            &with(|v| v["auth"] = json!({"type": "api_key", "in": "cookie", "name": "k"})),
            code::INVALID_VALUE,
            "/auth/in",
        );
        assert_issue(
            &with(|v| v["auth"] = json!({"type": "api_key", "in": "header", "name": "bad name"})),
            code::INVALID_HEADER,
            "/auth/name",
        );
        assert_issue(
            &with(|v| {
                v["auth"] = json!({"type": "oauth2_client_credentials", "token_url": "nope"});
            }),
            code::INVALID_URL,
            "/auth/token_url",
        );
        assert_issue(
            &with(|v| {
                v["auth"] = json!({"type": "oauth2_authorization_code",
                    "token_url": "https://a.test/t"});
            }),
            code::REQUIRED,
            "/auth/authorize_url",
        );
        assert_issue(
            &with(|v| {
                v["auth"] = json!({"type": "oauth2_client_credentials",
                    "token_url": "https://a.test/t", "client_auth": "header"});
            }),
            code::INVALID_VALUE,
            "/auth/client_auth",
        );
        assert_issue(
            &with(|v| v["auth"] = json!({"type": "bearer", "token": "inline-secret"})),
            code::UNKNOWN_FIELD,
            "/auth/token",
        );
    }

    #[test]
    fn the_rate_limit_is_bounded() {
        assert_valid(&with(|v| {
            v["rate_limit"] = json!({"requests": 1000, "per_seconds": 3600})
        }));
        for (requests, per, code, path) in [
            (0, 1, code::OUT_OF_RANGE, "/rate_limit/requests"),
            (1001, 1, code::OUT_OF_RANGE, "/rate_limit/requests"),
            (1, 0, code::OUT_OF_RANGE, "/rate_limit/per_seconds"),
            (1, 3601, code::OUT_OF_RANGE, "/rate_limit/per_seconds"),
        ] {
            assert_issue(
                &with(|v| v["rate_limit"] = json!({"requests": requests, "per_seconds": per})),
                code,
                path,
            );
        }
        assert_issue(
            &with(|v| v["rate_limit"] = json!({"requests": 5})),
            code::REQUIRED,
            "/rate_limit/per_seconds",
        );
    }

    #[test]
    fn headers_are_static_few_and_never_carry_auth() {
        assert_valid(&with(|v| {
            v["headers"] = json!({"Accept": "application/json"})
        }));
        for name in [
            "Authorization",
            "proxy-authorization",
            "Cookie",
            "Host",
            "Content-Length",
            "Transfer-Encoding",
            "Connection",
            "Range",
            "If-Match",
            "if-none-match",
        ] {
            assert_issue(
                &with(|v| v["headers"] = json!({ name: "x" })),
                code::FORBIDDEN_HEADER,
                &format!("/headers/{name}"),
            );
        }
        let many: serde_json::Map<String, Value> =
            (0..11).map(|n| (format!("X-H{n}"), json!("v"))).collect();
        assert_issue(
            &with(|v| v["headers"] = Value::Object(many)),
            code::TOO_MANY_HEADERS,
            "/headers",
        );
        assert_issue(
            &with(|v| v["headers"] = json!({"bad name": "v"})),
            code::INVALID_HEADER,
            "/headers/bad name",
        );
        assert_issue(
            &with(|v| v["headers"] = json!({"X-A": "line\nbreak"})),
            code::INVALID_HEADER,
            "/headers/X-A",
        );
    }

    #[test]
    fn a_stream_has_a_unique_name_and_a_clean_path() {
        assert_issue(
            &with(|v| {
                let first = v["streams"][0].clone();
                v["streams"].as_array_mut().unwrap().push(first);
            }),
            code::DUPLICATE_STREAM,
            "/streams/1/name",
        );
        assert_issue(
            &with(|v| v["streams"][0]["name"] = json!("")),
            code::INVALID_LENGTH,
            "/streams/0/name",
        );
        for bad in [
            "contacts",
            "/a//b",
            "/a/../b",
            "/a?x=1",
            "/a#x",
            "/a\\b",
            "http://x/y",
        ] {
            assert_issue(
                &with(|v| v["streams"][0]["path"] = json!(bad)),
                code::PATH_INVALID,
                "/streams/0/path",
            );
        }
        assert_issue(
            &with(|v| v["streams"][0]["path"] = json!(format!("/{}", "a".repeat(600)))),
            code::PATH_INVALID,
            "/streams/0/path",
        );
        assert_valid(&with(|v| {
            v["streams"][0]["query"] = json!({"limit": 10, "active": true, "q": "x"});
            v["streams"][0]["records_path"] = json!("/data/items");
        }));
        assert_issue(
            &with(|v| v["streams"][0]["query"] = json!({"a": [1]})),
            code::INVALID_TYPE,
            "/streams/0/query/a",
        );
        let many: serde_json::Map<String, Value> =
            (0..21).map(|n| (format!("p{n}"), json!(1))).collect();
        assert_issue(
            &with(|v| v["streams"][0]["query"] = Value::Object(many)),
            code::TOO_MANY_QUERY,
            "/streams/0/query",
        );
        assert_issue(
            &with(|v| v["streams"][0]["records_path"] = json!("data")),
            code::INVALID_POINTER,
            "/streams/0/records_path",
        );
    }

    #[test]
    fn a_stream_path_never_holds_a_percent_encoded_dot_segment_or_slash() {
        for bad in ["/a/%2e%2e/b", "/a/%2E%2E/b", "/a/%2e/b", "/a%2fb"] {
            assert_issue(
                &with(|v| v["streams"][0]["path"] = json!(bad)),
                code::PATH_INVALID,
                "/streams/0/path",
            );
        }
    }

    #[test]
    fn an_api_key_header_is_held_to_the_same_forbidden_list_as_the_headers_map() {
        for name in [
            "Host",
            "Transfer-Encoding",
            "Content-Length",
            "Connection",
            "Cookie",
            "If-Match",
        ] {
            assert_issue(
                &with(|v| v["auth"] = json!({"type": "api_key", "in": "header", "name": name})),
                code::FORBIDDEN_HEADER,
                "/auth/name",
            );
        }
        for name in ["X-Api-Key", "Authorization"] {
            assert_valid(&with(|v| {
                v["auth"] = json!({"type": "api_key", "in": "header", "name": name});
            }));
        }
        assert_valid(&with(|v| {
            v["auth"] = json!({"type": "api_key", "in": "query", "name": "host"});
        }));
    }

    #[test]
    fn every_pagination_style_is_accepted() {
        let styles = [
            json!({"type": "none"}),
            json!({"type": "cursor", "param": "cursor", "next": "/meta/next"}),
            json!({"type": "offset", "param": "offset", "limit_param": "limit", "limit": 50}),
            json!({"type": "page", "param": "page"}),
            json!({"type": "page", "param": "page", "first": 0,
                   "size_param": "per_page", "size": 100}),
            json!({"type": "link_header"}),
        ];
        for style in styles {
            assert_valid(&with(|v| v["streams"][0]["pagination"] = style.clone()));
        }
    }

    #[test]
    fn a_bad_pagination_names_the_key() {
        let at = |tail: &str| format!("/streams/0/pagination{tail}");
        assert_issue(
            &with(|v| v["streams"][0]["pagination"] = json!({"type": "scroll"})),
            code::INVALID_VALUE,
            &at("/type"),
        );
        assert_issue(
            &with(|v| v["streams"][0]["pagination"] = json!({"type": "cursor", "param": "c"})),
            code::REQUIRED,
            &at("/next"),
        );
        assert_issue(
            &with(|v| {
                v["streams"][0]["pagination"] = json!({"type": "cursor", "param": "c", "next": ""});
            }),
            code::INVALID_POINTER,
            &at("/next"),
        );
        for limit in [0, 1001] {
            assert_issue(
                &with(|v| {
                    v["streams"][0]["pagination"] = json!({"type": "offset", "param": "o",
                        "limit_param": "l", "limit": limit});
                }),
                code::OUT_OF_RANGE,
                &at("/limit"),
            );
        }
        assert_issue(
            &with(|v| {
                v["streams"][0]["pagination"] =
                    json!({"type": "page", "param": "p", "size_param": "s"});
            }),
            code::REQUIRED,
            &at("/size"),
        );
        assert_issue(
            &with(|v| {
                v["streams"][0]["pagination"] = json!({"type": "link_header", "param": "p"});
            }),
            code::UNKNOWN_FIELD,
            &at("/param"),
        );
    }

    #[test]
    fn fields_are_typed_named_and_unique() {
        assert_issue(
            &with(|v| v["streams"][0]["fields"][1]["name"] = json!("id")),
            code::DUPLICATE_FIELD,
            "/streams/0/fields/1/name",
        );
        assert_issue(
            &with(|v| v["streams"][0]["fields"][1]["name"] = json!("  ")),
            code::INVALID_FIELD_NAME,
            "/streams/0/fields/1/name",
        );
        assert_issue(
            &with(|v| v["streams"][0]["fields"][1]["type"] = json!("date")),
            code::INVALID_VALUE,
            "/streams/0/fields/1/type",
        );
        assert_issue(
            &with(|v| v["streams"][0]["fields"][1] = json!({"name": "x"})),
            code::REQUIRED,
            "/streams/0/fields/1/type",
        );
        assert_issue(
            &with(|v| v["streams"][0]["fields"] = json!([])),
            code::OUT_OF_RANGE,
            "/streams/0/fields",
        );
        for kind in [
            "string",
            "number",
            "boolean",
            "timestamp",
            "json",
            "unknown",
        ] {
            assert_valid(&with(|v| {
                v["streams"][0]["fields"][1]["type"] = json!(kind)
            }));
        }
    }

    #[test]
    fn the_primary_key_names_fields_and_is_never_empty() {
        assert_issue(
            &with(|v| v["streams"][0]["primary_key"] = json!(["id", "nope"])),
            code::PRIMARY_KEY_UNKNOWN_FIELD,
            "/streams/0/primary_key/1",
        );
        assert_issue(
            &with(|v| v["streams"][0]["primary_key"] = json!([])),
            code::PRIMARY_KEY_EMPTY,
            "/streams/0/primary_key",
        );
        assert_issue(
            &with(|v| {
                v["streams"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("primary_key");
            }),
            code::REQUIRED,
            "/streams/0/primary_key",
        );
        assert_valid(&with(|v| {
            v["streams"][0]["primary_key"] = json!(["id", "email"])
        }));
    }

    #[test]
    fn incremental_names_a_known_cursor_field_and_a_type() {
        let good = json!({"cursor_field": "updated_at", "request_param": "since",
                          "cursor_type": "timestamp"});
        assert_valid(&with(|v| v["streams"][0]["incremental"] = good.clone()));
        assert_issue(
            &with(|v| {
                v["streams"][0]["incremental"] = json!({"cursor_field": "ghost",
                    "request_param": "since", "cursor_type": "timestamp"});
            }),
            code::CURSOR_FIELD_UNKNOWN,
            "/streams/0/incremental/cursor_field",
        );
        assert_issue(
            &with(|v| {
                v["streams"][0]["incremental"] = json!({"cursor_field": "updated_at",
                    "request_param": "since", "cursor_type": "date"});
            }),
            code::INVALID_VALUE,
            "/streams/0/incremental/cursor_type",
        );
        assert_issue(
            &with(|v| {
                v["streams"][0]["incremental"] =
                    json!({"cursor_field": "updated_at", "cursor_type": "integer"});
            }),
            code::REQUIRED,
            "/streams/0/incremental/request_param",
        );
    }

    #[test]
    fn a_delete_signal_compares_a_path_to_a_literal() {
        for equals in [json!("deleted"), json!(1), json!(true)] {
            assert_valid(&with(|v| {
                v["streams"][0]["delete_signal"] = json!({"path": "status", "equals": equals});
            }));
        }
        assert_issue(
            &with(|v| v["streams"][0]["delete_signal"] = json!({"path": "s", "equals": [1]})),
            code::INVALID_TYPE,
            "/streams/0/delete_signal/equals",
        );
        assert_issue(
            &with(|v| v["streams"][0]["delete_signal"] = json!({"equals": 1})),
            code::REQUIRED,
            "/streams/0/delete_signal/path",
        );
        assert_issue(
            &with(|v| v["streams"][0]["delete_signal"] = json!({"path": "s"})),
            code::REQUIRED,
            "/streams/0/delete_signal/equals",
        );
    }

    fn mapping(columns: Value) -> Value {
        json!({"key_column": "id", "columns": columns})
    }

    #[test]
    fn a_mapping_is_the_one_the_editor_saves_with_dotted_paths() {
        assert_valid(&with(|v| {
            v["streams"][0]["fields"]
                .as_array_mut()
                .unwrap()
                .push(json!({"name": "address", "type": "json"}));
            v["streams"][0]["mapping"] = mapping(json!([
                {"column": "email", "target": {"type": "identifier", "kind": "email"},
                 "transforms": [{"op": "lower"}, {"op": "trim"}]},
                {"column": "address.city", "target": {"type": "trait", "key": "city"}},
            ]));
        }));
    }

    #[test]
    fn a_bad_mapping_is_refused_with_the_mapping_codes() {
        let at = "/streams/0/mapping";
        assert_issue(
            &with(|v| {
                v["streams"][0]["mapping"] = mapping(json!([
                    {"column": "email", "target": {"type": "identifier", "kind": "email"}},
                    {"column": "id", "target": {"type": "list",
                        "list_id": "00000000-0000-0000-0000-000000000000"}},
                ]));
            }),
            code::LIST_TARGET_NOT_ALLOWED,
            "/streams/0/mapping/columns/1/target",
        );
        assert_issue(
            &with(|v| {
                v["streams"][0]["mapping"] = mapping(json!([
                    {"column": "ghost", "target": {"type": "identifier", "kind": "email"}},
                ]));
            }),
            "unknown_column",
            at,
        );
        assert_issue(
            &with(|v| {
                v["streams"][0]["mapping"] = mapping(json!([
                    {"column": "email", "target": {"type": "ignore"}},
                ]));
            }),
            "no_identifier",
            at,
        );
        assert_issue(
            &with(|v| v["streams"][0]["mapping"] = json!({"columns": "no"})),
            code::INVALID_MAPPING,
            at,
        );
        assert_issue(
            &with(|v| v["streams"][0]["mapping"] = json!([])),
            code::INVALID_TYPE,
            at,
        );
        assert_issue(
            &with(|v| {
                v["streams"][0]["mapping"] = mapping(json!([
                    {"column": "email", "colum": "typo",
                     "target": {"type": "identifier", "kind": "email"}},
                ]));
            }),
            code::UNKNOWN_FIELD,
            "/streams/0/mapping/columns/0/colum",
        );
    }

    #[test]
    fn an_unknown_key_is_refused_wherever_it_is() {
        assert_issue(
            &with(|v| v["extra"] = json!(1)),
            code::UNKNOWN_FIELD,
            "/extra",
        );
        assert_issue(
            &with(|v| v["streams"][0]["extra"] = json!(1)),
            code::UNKNOWN_FIELD,
            "/streams/0/extra",
        );
        assert_issue(
            &with(|v| v["streams"][0]["fields"][0]["extra"] = json!(1)),
            code::UNKNOWN_FIELD,
            "/streams/0/fields/0/extra",
        );
        assert_issue(
            &with(|v| v["auth"]["extra"] = json!(1)),
            code::UNKNOWN_FIELD,
            "/auth/extra",
        );
    }

    #[test]
    fn a_script_like_key_is_not_supported() {
        for key in SCRIPT_KEYS {
            assert_issue(
                &with(|v| v["streams"][0][*key] = json!("anything")),
                code::EXPRESSION_NOT_SUPPORTED,
                &format!("/streams/0/{key}"),
            );
            assert_issue(
                &with(|v| v[*key] = json!({"a": 1})),
                code::EXPRESSION_NOT_SUPPORTED,
                &format!("/{key}"),
            );
        }
        assert_issue(
            &with(|v| {
                v["streams"][0]["mapping"] = mapping(json!([
                    {"column": "email", "script": "x",
                     "target": {"type": "identifier", "kind": "email"}},
                ]));
            }),
            code::EXPRESSION_NOT_SUPPORTED,
            "/streams/0/mapping/columns/0/script",
        );
    }

    #[test]
    fn a_template_marker_in_a_value_is_not_supported() {
        for marker in ["{{ now }}", "${HOME}", "{% if %}", "<% x %>"] {
            assert_issue(
                &with(|v| v["streams"][0]["query"] = json!({"q": format!("a {marker} b")})),
                code::EXPRESSION_NOT_SUPPORTED,
                "/streams/0/query/q",
            );
            assert_issue(
                &with(|v| v["headers"] = json!({"X-A": marker})),
                code::EXPRESSION_NOT_SUPPORTED,
                "/headers/X-A",
            );
        }
    }

    #[test]
    fn a_manifest_over_a_limit_is_refused_without_being_walked() {
        // Each of these also holds an error the walk would find.
        let broken = |edit: &dyn Fn(&mut Value)| {
            let mut value = with(|v| v["version"] = json!("v9"));
            edit(&mut value);
            found(&value)
        };

        let large = broken(&|v| v["padding"] = json!("x".repeat(70_000)));
        assert_eq!(large, vec![(code::MANIFEST_TOO_LARGE, String::new())]);

        let streams = broken(&|v| {
            let stream = v["streams"][0].clone();
            v["streams"] = Value::Array(vec![stream; 21]);
        });
        assert_eq!(
            streams,
            vec![(code::TOO_MANY_STREAMS, "/streams".to_owned())]
        );

        let fields = broken(&|v| {
            v["streams"][0]["fields"] = Value::Array(
                (0..201)
                    .map(|n| json!({"name": format!("f{n}"), "type": "string"}))
                    .collect(),
            );
        });
        assert_eq!(
            fields,
            vec![(code::TOO_MANY_FIELDS, "/streams/0/fields".to_owned())]
        );

        let deep = broken(&|v| {
            let mut nested = json!(1);
            for _ in 0..12 {
                nested = json!({"a": nested});
            }
            v["deep"] = nested;
        });
        assert_eq!(deep, vec![(code::TOO_DEEP, String::new())]);
    }

    #[test]
    fn exactly_twenty_streams_two_hundred_fields_and_twelve_levels_are_within_the_limits() {
        let mut value = valid();
        let stream = value["streams"][0].clone();
        value["streams"] = Value::Array(
            (0..20)
                .map(|n| {
                    let mut stream = stream.clone();
                    stream["name"] = json!(format!("s{n}"));
                    stream
                })
                .collect(),
        );
        value["streams"][0]["fields"] = Value::Array(
            (0..200)
                .map(|n| json!({"name": format!("f{n}"), "type": "string"}))
                .collect(),
        );
        value["streams"][0]["primary_key"] = json!(["f0"]);
        assert_valid(&value);

        // Eleven levels below the root is twelve in all: the limit itself.
        let mut nested = json!(1);
        for _ in 0..11 {
            nested = json!({"a": nested});
        }
        let mut value = valid();
        value["deep"] = nested;
        assert!(!found(&value).iter().any(|(c, _)| *c == code::TOO_DEEP));
    }

    #[test]
    fn no_issue_holds_a_value_of_the_manifest() {
        const SENTINEL: &str = "SENTINEL-VALUE-9f2c1";
        let sentinel = json!(SENTINEL);
        let broken = [
            with(|v| v["version"] = sentinel.clone()),
            with(|v| v["base_url"] = sentinel.clone()),
            with(|v| v["auth"] = json!({"type": SENTINEL})),
            with(|v| v["auth"] = json!({"type": "api_key", "in": SENTINEL, "name": SENTINEL})),
            with(|v| v["headers"] = json!({"X-A": SENTINEL, "Authorization": SENTINEL})),
            with(|v| {
                v["streams"][0]["name"] = sentinel.clone();
                let again = v["streams"][0].clone();
                v["streams"].as_array_mut().unwrap().push(again);
            }),
            with(|v| v["streams"][0]["path"] = sentinel.clone()),
            with(|v| v["streams"][0]["query"] = json!({"a": [SENTINEL]})),
            with(|v| v["streams"][0]["records_path"] = sentinel.clone()),
            with(|v| v["streams"][0]["primary_key"] = json!([SENTINEL])),
            with(|v| v["streams"][0]["fields"][1]["type"] = sentinel.clone()),
            with(|v| {
                v["streams"][0]["incremental"] = json!({"cursor_field": SENTINEL,
                    "request_param": 1, "cursor_type": SENTINEL});
            }),
            with(|v| {
                v["streams"][0]["delete_signal"] = json!({"path": 1, "equals": [SENTINEL]});
            }),
            with(|v| v["streams"][0]["mapping"] = json!({"columns": SENTINEL})),
            with(|v| {
                v["streams"][0]["mapping"] = mapping(json!([
                    {"column": SENTINEL, "target": {"type": "identifier", "kind": "email"}}
                ]));
            }),
            with(|v| {
                v["streams"][0]["pagination"] = json!({"type": "cursor", "param": 1,
                "next": SENTINEL})
            }),
            with(|v| v["streams"][0]["x"] = json!("{{ SENTINEL-VALUE-9f2c1 }}")),
        ];
        for value in broken {
            let issues = validate(&value).unwrap_err();
            assert!(!issues.is_empty());
            for issue in issues {
                let shown = format!("{issue:?} {issue} {}", issue.code);
                assert!(!shown.contains(SENTINEL), "{shown}");
            }
        }
    }

    #[test]
    fn a_manifest_with_many_errors_reports_a_bounded_list() {
        let streams: Vec<Value> = (0..20)
            .map(|n| {
                json!({"name": format!("s{n}"), "path": "bad", "fields": [],
                            "primary_key": [], "x": 1, "y": 2})
            })
            .collect();
        let value = with(|v| v["streams"] = Value::Array(streams));

        let issues = validate(&value).unwrap_err();

        assert_eq!(issues.len(), MAX_ISSUES);
    }

    #[test]
    fn each_auth_type_declares_the_credential_fields_it_reads() {
        use super::super::{declared_credential_fields, optional_credential_fields};
        let cases = [
            (json!({"type": "none"}), vec![], vec![]),
            (
                json!({"type": "api_key", "in": "query", "name": "k"}),
                vec!["api_key"],
                vec![],
            ),
            (
                json!({"type": "basic"}),
                vec!["username", "password"],
                vec![],
            ),
            (json!({"type": "bearer"}), vec!["token"], vec![]),
            (
                json!({"type": "oauth2_client_credentials", "token_url": "https://a.test/t"}),
                vec!["client_id", "client_secret"],
                vec![],
            ),
            (
                json!({"type": "oauth2_authorization_code",
                       "authorize_url": "https://a.test/a", "token_url": "https://a.test/t"}),
                vec!["client_id", "client_secret"],
                vec!["client_secret"],
            ),
        ];
        for (auth, declared, optional) in cases {
            let manifest = validate(&with(|v| v["auth"] = auth.clone())).unwrap();
            assert_eq!(declared_credential_fields(&manifest), declared.as_slice());
            assert_eq!(optional_credential_fields(&manifest), optional.as_slice());
        }
    }

    #[test]
    fn only_the_authorization_code_grant_needs_a_consent() {
        use super::super::needs_authorization;

        assert!(needs_authorization(
            &json!({"auth": {"type": "oauth2_authorization_code"}})
        ));
        for other in [
            json!({"auth": {"type": "bearer"}}),
            json!({"auth": {}}),
            json!({"auth": 3}),
            json!({}),
            json!(null),
        ] {
            assert!(!needs_authorization(&other), "{other}");
        }
    }

    #[test]
    fn the_parsed_manifest_carries_the_checked_values() {
        let value: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/manifests/cursor_oauth2_code.json"
        ))
        .unwrap();

        let manifest = validate(&value).unwrap();
        let stream = &manifest.streams[0];

        assert_eq!(manifest.base_url.as_str(), "https://api.example.test/v1");
        assert!(matches!(
            manifest.auth,
            Auth::OAuth2AuthorizationCode { .. }
        ));
        assert_eq!(manifest.rate_limit.unwrap().requests, 100);
        assert_eq!(stream.query, vec![("limit".to_owned(), "2".to_owned())]);
        assert_eq!(stream.records_path, "/data");
        assert_eq!(stream.primary_key, vec!["id".to_owned()]);
        assert_eq!(
            stream.pagination,
            PaginationSpec::Cursor {
                param: "cursor".to_owned(),
                next: "/next_cursor".to_owned()
            }
        );
        assert_eq!(
            stream.incremental.as_ref().unwrap().cursor_type,
            CursorType::Timestamp
        );
        assert_eq!(
            stream.delete_signal.as_ref().unwrap().equals,
            json!("deleted")
        );
        assert!(stream.mapping.is_some());
    }

    #[test]
    fn a_manifest_names_a_built_in_credential_kind_or_declares_its_own_fields() {
        use zirv_flow_core::credentials::CredentialKind;

        let named = validate(&with(|v| {
            v["auth"] = json!({"type": "basic", "credential": {"kind": "basic"}});
        }))
        .unwrap();
        assert_eq!(
            named.credential,
            Some(CredentialSpec::Kind(CredentialKind::Basic))
        );

        let declared = validate(&with(|v| {
            v["auth"] = json!({"type": "bearer", "credential": {"fields": [
                {"key": "token", "label": "Token", "type": "string", "required": true, "secret": true},
                {"key": "region", "label": "Region", "type": "enum", "options": ["eu", "us"]}
            ]}});
        }))
        .unwrap();
        let schema = declared.credential.unwrap().schema();
        assert_eq!(schema.fields.len(), 2);
        assert!(schema.field("token").unwrap().secret);
        assert_eq!(schema.field("region").unwrap().options, ["eu", "us"]);

        assert!(
            validate(&with(|v| v["auth"] = json!({"type": "none"})))
                .unwrap()
                .credential
                .is_none()
        );
    }

    #[test]
    fn a_declared_credential_schema_survives_a_round_trip_through_json() {
        let spec = json!({"fields": [
            {"key": "token", "label": "Token", "type": "string", "required": true, "secret": true}
        ]});
        let manifest = validate(&with(|v| {
            v["auth"] = json!({"type": "bearer", "credential": spec});
        }))
        .unwrap();

        let schema = manifest.credential.unwrap().schema();

        assert_eq!(
            serde_json::to_value(&schema).unwrap(),
            json!({"fields": [
                {"key": "token", "label": "Token", "type": "string", "required": true, "secret": true}
            ]})
        );
    }

    #[test]
    fn a_bad_credential_section_names_the_key() {
        let field = json!({"key": "token", "label": "T", "type": "string", "secret": true});
        let cases = [
            (
                json!({"kind": "nope"}),
                "/auth/credential/kind",
                code::INVALID_VALUE,
            ),
            (
                json!({"kind": 3}),
                "/auth/credential/kind",
                code::INVALID_TYPE,
            ),
            (json!({}), "/auth/credential/kind", code::REQUIRED),
            (
                json!({"kind": "basic", "fields": [field.clone()]}),
                "/auth/credential/fields",
                code::INVALID_VALUE,
            ),
            (
                json!({"fields": "x"}),
                "/auth/credential/fields",
                code::INVALID_TYPE,
            ),
            (
                json!({"fields": [{"key": "Bad Key", "label": "T", "type": "string"}]}),
                "/auth/credential/fields",
                code::INVALID_CREDENTIAL_SCHEMA,
            ),
            (
                json!({"fields": [{"key": "a", "label": "T", "type": "string", "extra": 1}]}),
                "/auth/credential/fields",
                code::INVALID_CREDENTIAL_SCHEMA,
            ),
            (
                json!({"fields": [{"key": "other", "label": "T", "type": "string"}]}),
                "/auth/credential",
                code::CREDENTIAL_FIELD_MISSING,
            ),
            (
                json!({"kind": "api_key", "zzz": 1}),
                "/auth/credential/zzz",
                code::UNKNOWN_FIELD,
            ),
        ];
        for (credential, path, issue_code) in cases {
            assert_issue(
                &with(|v| v["auth"] = json!({"type": "bearer", "credential": credential.clone()})),
                issue_code,
                path,
            );
        }
    }
}
