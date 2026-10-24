//! The `destination` section of a manifest (architecture "Provider-manifest destinations", #223):
//! how to send an email through an HTTP API, how to read its errors, how to ask it for the state
//! of a sending domain, and how to read and verify its feedback webhook. Like the rest of the
//! manifest it is data only: closed sets of keys, JSON pointers, lists of literal values. There is
//! no expression language, and a template marker anywhere is refused by the manifest-wide scan.
//!
//! This module is the model and the validator. The runtime that executes it is
//! `destinations::provider`.

use serde_json::{Map, Value};

use super::issue::{child, code, index};
use super::model::{Auth, CredentialSpec, KeyLocation, RateLimitSpec};
use super::parse::{MAX_NAME, MAX_PATH, Obj, Walk, is_forbidden_header, is_token, rate_limit};

/// The most error rules a manifest lists.
const MAX_ERROR_RULES: usize = 20;
/// The most values in one list of literals.
const MAX_VALUES: usize = 20;
/// The longest literal in such a list.
const MAX_VALUE_CHARS: usize = 128;
/// The most static values a send body carries.
const MAX_STATIC: usize = 20;
/// The most tokens in a body placement.
const MAX_POINTER_TOKENS: usize = 8;
/// The highest array position a placement may name.
const MAX_ARRAY_INDEX: usize = 31;
/// The default tolerance of a timestamped feedback signature, in seconds.
const DEFAULT_TOLERANCE_SECONDS: u32 = 300;

/// A provider destination: the `destination` section of a validated manifest.
#[derive(Clone, Debug)]
pub struct Provider {
    pub send: SendSpec,
    /// Feeds the destination's rate bucket when the destination is created without a rate.
    pub rate_limit: Option<RateLimitSpec>,
    /// Checked in order; the first rule that matches wins. A response no rule matches is judged
    /// by the defaults (408, 429 and 5xx retry; any other status is terminal).
    pub errors: Vec<ErrorRule>,
    pub domain_status: Option<DomainStatusSpec>,
    pub feedback: FeedbackSpec,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SendMethod {
    Post,
    Put,
    Patch,
}

/// The message fields a send body can carry. The set is closed.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MessageField {
    /// The sender's address.
    From,
    /// The sender's display name.
    FromName,
    ReplyTo,
    /// The recipient's address.
    To,
    Subject,
    Html,
    Text,
    /// The extra headers of the message, as an object of name to value.
    Headers,
    /// Our message id: the provider echoes it in its feedback.
    MessageId,
}

impl MessageField {
    /// Every field with its key in the manifest, in the order they are read.
    pub const ALL: [(&'static str, MessageField); 9] = [
        ("from", Self::From),
        ("from_name", Self::FromName),
        ("reply_to", Self::ReplyTo),
        ("to", Self::To),
        ("subject", Self::Subject),
        ("html", Self::Html),
        ("text", Self::Text),
        ("headers", Self::Headers),
        ("message_id", Self::MessageId),
    ];

    fn required(self) -> bool {
        matches!(
            self,
            Self::From | Self::To | Self::Subject | Self::MessageId
        )
    }
}

/// How one email is sent.
#[derive(Clone, Debug)]
pub struct SendSpec {
    pub method: SendMethod,
    /// Starts with `/`, relative to the base URL's path.
    pub path: String,
    /// The header that carries the message id as the idempotency key.
    pub idempotency_header: String,
    /// Where each message field is placed in the JSON body, as a JSON pointer.
    pub fields: Vec<(MessageField, String)>,
    /// Literal values placed in the body beside the message fields, by JSON pointer.
    pub statics: Vec<(String, Value)>,
}

/// The statuses a rule matches: `min..=max`, both within 400 to 599.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StatusRange {
    pub min: u16,
    pub max: u16,
}

impl StatusRange {
    pub fn contains(self, status: u16) -> bool {
        (self.min..=self.max).contains(&status)
    }
}

/// A rule's condition on the response body: the JSON value at `pointer` is one of `values`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BodyMatch {
    pub pointer: String,
    pub values: Vec<String>,
}

/// What a matching rule makes of a failed response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorAction {
    /// Try again later (and honour `Retry-After`).
    Retry,
    /// Retrying cannot help; nobody is suppressed.
    Terminal,
    /// The provider says the recipient's address cannot receive mail: a hard bounce.
    RecipientFailure,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ErrorRule {
    pub status: StatusRange,
    pub body: Option<BodyMatch>,
    pub action: ErrorAction,
}

/// The request that asks the provider for the state of a sending domain, and how its answer maps
/// to the records of the domain. The domain is the only dynamic value, sent as one query
/// parameter of a GET.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DomainStatusSpec {
    pub path: String,
    pub domain_param: String,
    /// A pointer to the list of records; empty for a top-level array.
    pub records_path: String,
    /// Pointers inside one record.
    pub name: String,
    pub record_type: String,
    pub status: String,
    /// The provider's status values that mean `verified`, `pending` and `failed`.
    pub verified: Vec<String>,
    pub pending: Vec<String>,
    pub failed: Vec<String>,
}

/// How to read the bounce kind of a bounce event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BounceKindSpec {
    pub pointer: String,
    pub hard: Vec<String>,
    pub soft: Vec<String>,
}

/// How the feedback webhook's signature is verified: HMAC-SHA256 under the destination's secret,
/// mapped onto `zirv_flow_core::signature::Scheme`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerificationSpec {
    pub signature_header: String,
    pub signature_prefix: String,
    /// The header holding the unix seconds the provider signed; none selects the body-only scheme.
    pub timestamp_header: Option<String>,
    pub tolerance_seconds: u32,
    /// The credential field holding the signing secret.
    pub secret_field: String,
}

/// How to read one feedback event from the webhook body (a single JSON object).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeedbackSpec {
    /// A pointer to the event type.
    pub event_type: String,
    /// The provider's event types that mean `delivered`, `bounced` and `complained`.
    pub delivered: Vec<String>,
    pub bounced: Vec<String>,
    pub complained: Vec<String>,
    /// A pointer to our message id, as the send placed it.
    pub message_id: String,
    pub bounce_kind: Option<BounceKindSpec>,
    /// A pointer to the provider's id of the event, for deduplication.
    pub event_id: Option<String>,
    pub verification: VerificationSpec,
}

// ---------------------------------------------------------------------------------------------
// Placement of a value in a JSON body.
// ---------------------------------------------------------------------------------------------

/// Why a value could not be placed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaceError;

fn unescape(token: &str) -> String {
    token.replace("~1", "/").replace("~0", "~")
}

/// Whether a pointer token names an array position: canonical decimal digits.
fn array_index(token: &str) -> Option<usize> {
    let digits = !token.is_empty() && token.bytes().all(|byte| byte.is_ascii_digit());
    let canonical = token == "0" || !token.starts_with('0');
    (digits && canonical).then(|| token.parse().ok()).flatten()
}

/// Whether `pointer` is a body placement: not the root, at most [`MAX_POINTER_TOKENS`] tokens,
/// no empty token, only the two escapes, array positions up to [`MAX_ARRAY_INDEX`].
fn valid_placement(pointer: &str) -> bool {
    if pointer.len() > MAX_PATH || !pointer.starts_with('/') {
        return false;
    }
    let tokens: Vec<&str> = pointer[1..].split('/').collect();
    if tokens.len() > MAX_POINTER_TOKENS {
        return false;
    }
    tokens.iter().all(|token| {
        let escapes_ok = token
            .match_indices('~')
            .all(|(at, _)| matches!(token.as_bytes().get(at + 1), Some(b'0' | b'1')));
        !token.is_empty()
            && escapes_ok
            && !token.chars().any(char::is_control)
            && array_index(token).is_none_or(|position| position <= MAX_ARRAY_INDEX)
    })
}

/// Places `value` at `pointer` in `root`, creating the objects and arrays on the way. A token of
/// canonical digits is an array position, and a position may only be the next one of its array.
/// A place that holds something already, or a container of the other kind, is an error.
pub fn place(root: &mut Value, pointer: &str, value: Value) -> Result<(), PlaceError> {
    let tokens: Vec<String> = pointer
        .strip_prefix('/')
        .ok_or(PlaceError)?
        .split('/')
        .map(unescape)
        .collect();
    let mut slot = root;
    for token in &tokens {
        let wants_array = array_index(token).is_some();
        if slot.is_null() {
            *slot = if wants_array {
                Value::Array(Vec::new())
            } else {
                Value::Object(Map::new())
            };
        }
        slot = match slot {
            Value::Object(map) if !wants_array => map.entry(token.clone()).or_insert(Value::Null),
            Value::Array(items) => {
                let at = array_index(token).ok_or(PlaceError)?;
                if at > items.len() {
                    return Err(PlaceError);
                }
                if at == items.len() {
                    items.push(Value::Null);
                }
                &mut items[at]
            }
            _ => return Err(PlaceError),
        };
    }
    if slot.is_null() {
        *slot = value;
        Ok(())
    } else {
        Err(PlaceError)
    }
}

// ---------------------------------------------------------------------------------------------
// The validator.
// ---------------------------------------------------------------------------------------------

/// A non-empty JSON pointer under `key`.
fn nonempty_pointer(walk: &mut Walk, obj: &mut Obj<'_>, key: &'static str) -> Option<String> {
    let at = obj.at(key);
    let pointer = walk.pointer(obj, key, true)?;
    if pointer.is_empty() {
        walk.add(code::INVALID_POINTER, at);
        return None;
    }
    Some(pointer.to_owned())
}

/// A list of 1 to [`MAX_VALUES`] literals, as text: strings, or numbers and booleans as written.
fn literals(walk: &mut Walk, value: &Value, path: &str) -> Vec<String> {
    let Some(items) = value.as_array() else {
        walk.add(code::INVALID_TYPE, path.to_owned());
        return Vec::new();
    };
    if items.is_empty() || items.len() > MAX_VALUES {
        walk.add(code::OUT_OF_RANGE, path.to_owned());
        return Vec::new();
    }
    let mut values = Vec::new();
    for (position, item) in items.iter().enumerate() {
        let at = index(path, position);
        match item {
            Value::String(text)
                if !text.is_empty()
                    && text.chars().count() <= MAX_VALUE_CHARS
                    && !text.chars().any(char::is_control) =>
            {
                values.push(text.clone());
            }
            Value::Number(_) | Value::Bool(_) => values.push(item.to_string()),
            Value::String(_) => walk.add(code::INVALID_LENGTH, at),
            _ => walk.add(code::INVALID_TYPE, at),
        }
    }
    values
}

/// An optional list of literals under `key`; empty when absent.
fn optional_literals(walk: &mut Walk, obj: &mut Obj<'_>, key: &'static str) -> Vec<String> {
    match obj.take(key) {
        Some(value) => literals(walk, value, &obj.at(key)),
        None => Vec::new(),
    }
}

pub(super) fn provider(walk: &mut Walk, value: &Value, path: String) -> Option<Provider> {
    let mut obj = walk.object(value, path)?;
    let send = match obj.take("send") {
        Some(value) => send(walk, value, obj.at("send")),
        None => {
            walk.missing(&obj, "send");
            None
        }
    };
    let rate_limit = obj
        .take("rate_limit")
        .and_then(|value| rate_limit(walk, value, obj.at("rate_limit")));
    let errors = obj
        .take("errors")
        .map(|value| errors(walk, value, obj.at("errors")))
        .unwrap_or_default();
    let domain_status = obj
        .take("domain_status")
        .and_then(|value| domain_status(walk, value, obj.at("domain_status")));
    let feedback = match obj.take("feedback") {
        Some(value) => feedback(walk, value, obj.at("feedback")),
        None => {
            walk.missing(&obj, "feedback");
            None
        }
    };
    walk.finish(obj);
    Some(Provider {
        send: send?,
        rate_limit,
        errors,
        domain_status,
        feedback: feedback?,
    })
}

fn send(walk: &mut Walk, value: &Value, path: String) -> Option<SendSpec> {
    let mut obj = walk.object(value, path)?;
    let method = match walk.choice(&mut obj, "method", false, &["POST", "PUT", "PATCH"]) {
        Some("PUT") => SendMethod::Put,
        Some("PATCH") => SendMethod::Patch,
        _ => SendMethod::Post,
    };
    let send_path = super::parse::stream_path(walk, &mut obj);
    let header_at = obj.at("idempotency_header");
    let header = walk
        .text(&mut obj, "idempotency_header", true, MAX_NAME)
        .and_then(|name| {
            if is_forbidden_header(name) {
                walk.add(code::FORBIDDEN_HEADER, header_at.clone());
                None
            } else if !is_token(name) {
                walk.add(code::INVALID_HEADER, header_at.clone());
                None
            } else {
                Some(name.to_owned())
            }
        });
    let body = match obj.take("body") {
        Some(value) => body(walk, value, obj.at("body")),
        None => {
            walk.missing(&obj, "body");
            None
        }
    };
    walk.finish(obj);
    let (fields, statics) = body?;
    Some(SendSpec {
        method,
        path: send_path?,
        idempotency_header: header?,
        fields,
        statics,
    })
}

type Body = (Vec<(MessageField, String)>, Vec<(String, Value)>);

fn body(walk: &mut Walk, value: &Value, path: String) -> Option<Body> {
    let mut obj = walk.object(value, path.clone())?;
    let fields = match obj.take("fields") {
        Some(value) => message_fields(walk, value, obj.at("fields")),
        None => {
            walk.missing(&obj, "fields");
            None
        }
    };
    let statics = obj
        .take("static")
        .map(|value| statics(walk, value, obj.at("static")))
        .unwrap_or_default();
    walk.finish(obj);
    let fields = fields?;
    // A dry run with placeholders finds two placements that collide.
    let mut probe = Value::Null;
    let placements = fields
        .iter()
        .map(|(_, pointer)| pointer.as_str())
        .chain(statics.iter().map(|(pointer, _)| pointer.as_str()));
    let mut collides = false;
    for pointer in placements {
        collides |= place(&mut probe, pointer, Value::Bool(true)).is_err();
    }
    if collides {
        walk.add(code::POINTER_CONFLICT, path);
        return None;
    }
    Some((fields, statics))
}

fn message_fields(
    walk: &mut Walk,
    value: &Value,
    path: String,
) -> Option<Vec<(MessageField, String)>> {
    let mut obj = walk.object(value, path)?;
    let mut fields = Vec::new();
    for (key, field) in MessageField::ALL {
        let at = obj.at(key);
        let Some(pointer) = walk.pointer(&mut obj, key, field.required()) else {
            continue;
        };
        if valid_placement(pointer) {
            fields.push((field, pointer.to_owned()));
        } else {
            walk.add(code::INVALID_POINTER, at);
        }
    }
    let has = |wanted: MessageField| fields.iter().any(|(field, _)| *field == wanted);
    if !has(MessageField::Html) && !has(MessageField::Text) {
        walk.missing(&obj, "html");
    }
    walk.finish(obj);
    Some(fields)
}

fn statics(walk: &mut Walk, value: &Value, path: String) -> Vec<(String, Value)> {
    let Some(map) = value.as_object() else {
        walk.add(code::INVALID_TYPE, path);
        return Vec::new();
    };
    if map.len() > MAX_STATIC {
        walk.add(code::OUT_OF_RANGE, path);
        return Vec::new();
    }
    let mut statics = Vec::new();
    for (pointer, value) in map {
        let at = child(&path, pointer);
        if !valid_placement(pointer) {
            walk.add(code::INVALID_POINTER, at);
            continue;
        }
        match value {
            Value::String(text) if text.len() <= MAX_PATH => {
                statics.push((pointer.clone(), value.clone()));
            }
            Value::Number(_) | Value::Bool(_) => statics.push((pointer.clone(), value.clone())),
            Value::String(_) => walk.add(code::INVALID_LENGTH, at),
            _ => walk.add(code::INVALID_TYPE, at),
        }
    }
    statics
}

fn errors(walk: &mut Walk, value: &Value, path: String) -> Vec<ErrorRule> {
    let Some(items) = value.as_array() else {
        walk.add(code::INVALID_TYPE, path);
        return Vec::new();
    };
    if items.len() > MAX_ERROR_RULES {
        walk.add(code::OUT_OF_RANGE, path);
        return Vec::new();
    }
    items
        .iter()
        .enumerate()
        .filter_map(|(position, item)| error_rule(walk, item, index(&path, position)))
        .collect()
}

/// A status rule: a number (`422`) or a range as text (`"500-599"`), within 400 to 599.
fn status_range(walk: &mut Walk, obj: &mut Obj<'_>) -> Option<StatusRange> {
    let at = obj.at("status");
    let Some(value) = obj.take("status") else {
        walk.missing(obj, "status");
        return None;
    };
    let range = match value {
        Value::Number(number) => number
            .as_u64()
            .and_then(|status| u16::try_from(status).ok())
            .map(|status| (status, status)),
        Value::String(text) => text
            .split_once('-')
            .and_then(|(low, high)| Some((low.trim().parse().ok()?, high.trim().parse().ok()?))),
        _ => {
            walk.add(code::INVALID_TYPE, at);
            return None;
        }
    };
    match range {
        Some((min, max)) if (400..=599).contains(&min) && (min..=599).contains(&max) => {
            Some(StatusRange { min, max })
        }
        _ => {
            walk.add(code::OUT_OF_RANGE, at);
            None
        }
    }
}

fn error_rule(walk: &mut Walk, value: &Value, path: String) -> Option<ErrorRule> {
    let mut obj = walk.object(value, path)?;
    let status = status_range(walk, &mut obj);
    let body = match obj.take("body") {
        Some(value) => body_match(walk, value, obj.at("body")),
        None => None,
    };
    let action = walk.choice(
        &mut obj,
        "action",
        true,
        &["retry", "terminal", "recipient_failure"],
    );
    walk.finish(obj);
    let action = match action? {
        "retry" => ErrorAction::Retry,
        "terminal" => ErrorAction::Terminal,
        _ => ErrorAction::RecipientFailure,
    };
    Some(ErrorRule {
        status: status?,
        body,
        action,
    })
}

fn body_match(walk: &mut Walk, value: &Value, path: String) -> Option<BodyMatch> {
    let mut obj = walk.object(value, path)?;
    let pointer = nonempty_pointer(walk, &mut obj, "pointer");
    let values = match obj.take("values") {
        Some(value) => literals(walk, value, &obj.at("values")),
        None => {
            walk.missing(&obj, "values");
            Vec::new()
        }
    };
    walk.finish(obj);
    (!values.is_empty()).then_some(())?;
    Some(BodyMatch {
        pointer: pointer?,
        values,
    })
}

fn domain_status(walk: &mut Walk, value: &Value, path: String) -> Option<DomainStatusSpec> {
    let mut obj = walk.object(value, path)?;
    let request_path = super::parse::stream_path(walk, &mut obj);
    let param_at = obj.at("domain_param");
    let domain_param = walk
        .text(&mut obj, "domain_param", true, MAX_NAME)
        .and_then(|name| {
            if is_token(name) {
                Some(name.to_owned())
            } else {
                walk.add(code::INVALID_VALUE, param_at.clone());
                None
            }
        });
    let records_path = walk.pointer(&mut obj, "records_path", false);
    let name = nonempty_pointer(walk, &mut obj, "name");
    let record_type = nonempty_pointer(walk, &mut obj, "type");
    let status = nonempty_pointer(walk, &mut obj, "status");
    let map_at = obj.at("status_map");
    let (verified, pending, failed) = match obj.take("status_map") {
        Some(value) => status_map(walk, value, map_at.clone()),
        None => {
            walk.missing(&obj, "status_map");
            (Vec::new(), Vec::new(), Vec::new())
        }
    };
    walk.finish(obj);
    if verified.is_empty() && pending.is_empty() && failed.is_empty() {
        walk.add(code::REQUIRED, map_at);
        return None;
    }
    Some(DomainStatusSpec {
        path: request_path?,
        domain_param: domain_param?,
        records_path: records_path.unwrap_or_default().to_owned(),
        name: name?,
        record_type: record_type?,
        status: status?,
        verified,
        pending,
        failed,
    })
}

type StatusMap = (Vec<String>, Vec<String>, Vec<String>);

fn status_map(walk: &mut Walk, value: &Value, path: String) -> StatusMap {
    let Some(mut obj) = walk.object(value, path) else {
        return (Vec::new(), Vec::new(), Vec::new());
    };
    let verified = optional_literals(walk, &mut obj, "verified");
    let pending = optional_literals(walk, &mut obj, "pending");
    let failed = optional_literals(walk, &mut obj, "failed");
    walk.finish(obj);
    (verified, pending, failed)
}

fn feedback(walk: &mut Walk, value: &Value, path: String) -> Option<FeedbackSpec> {
    let mut obj = walk.object(value, path)?;
    let event_type = nonempty_pointer(walk, &mut obj, "event_type");
    let events_at = obj.at("events");
    let (delivered, bounced, complained) = match obj.take("events") {
        Some(value) => {
            let mut events = walk.object(value, events_at.clone())?;
            let delivered = optional_literals(walk, &mut events, "delivered");
            let bounced = optional_literals(walk, &mut events, "bounced");
            let complained = optional_literals(walk, &mut events, "complained");
            walk.finish(events);
            (delivered, bounced, complained)
        }
        None => {
            walk.missing(&obj, "events");
            (Vec::new(), Vec::new(), Vec::new())
        }
    };
    if obj.map.contains_key("events")
        && delivered.is_empty()
        && bounced.is_empty()
        && complained.is_empty()
    {
        walk.add(code::REQUIRED, events_at);
    }
    let message_id = nonempty_pointer(walk, &mut obj, "message_id");
    let bounce_at = obj.at("bounce_kind");
    let bounce_kind = obj
        .take("bounce_kind")
        .and_then(|value| bounce_kind(walk, value, bounce_at.clone()));
    if !bounced.is_empty() && bounce_kind.is_none() && !obj.map.contains_key("bounce_kind") {
        walk.add(code::REQUIRED, bounce_at);
    }
    let event_id_at = obj.at("event_id");
    let event_id = match walk.pointer(&mut obj, "event_id", false) {
        Some("") => {
            walk.add(code::INVALID_POINTER, event_id_at);
            None
        }
        other => other.map(str::to_owned),
    };
    let verification = match obj.take("verification") {
        Some(value) => verification(walk, value, obj.at("verification")),
        None => {
            // A feedback endpoint that does not verify is an open door to suppress anyone.
            walk.missing(&obj, "verification");
            None
        }
    };
    walk.finish(obj);
    Some(FeedbackSpec {
        event_type: event_type?,
        delivered,
        bounced,
        complained,
        message_id: message_id?,
        bounce_kind,
        event_id,
        verification: verification?,
    })
}

fn bounce_kind(walk: &mut Walk, value: &Value, path: String) -> Option<BounceKindSpec> {
    let mut obj = walk.object(value, path)?;
    let pointer = nonempty_pointer(walk, &mut obj, "pointer");
    let hard = optional_literals(walk, &mut obj, "hard");
    let soft = optional_literals(walk, &mut obj, "soft");
    if hard.is_empty() && soft.is_empty() {
        walk.missing(&obj, "hard");
    }
    walk.finish(obj);
    Some(BounceKindSpec {
        pointer: pointer?,
        hard,
        soft,
    })
}

fn verification(walk: &mut Walk, value: &Value, path: String) -> Option<VerificationSpec> {
    let mut obj = walk.object(value, path)?;
    let header = header_name(walk, &mut obj, "signature_header", true);
    let prefix_at = obj.at("signature_prefix");
    let prefix = match obj.take("signature_prefix") {
        None => Some(String::new()),
        Some(Value::String(text))
            if text.len() <= zirv_flow_core::signature::MAX_HEADER_NAME_CHARS
                && text.is_ascii()
                && !text.chars().any(char::is_control) =>
        {
            Some(text.clone())
        }
        Some(Value::String(_)) => {
            walk.add(code::INVALID_LENGTH, prefix_at);
            None
        }
        Some(_) => {
            walk.add(code::INVALID_TYPE, prefix_at);
            None
        }
    };
    let timestamp = header_name(walk, &mut obj, "timestamp_header", false);
    let tolerance = walk.number(&mut obj, "tolerance_seconds", false, 1..=3600);
    let secret_at = obj.at("secret_field");
    let secret_field = walk
        .text(&mut obj, "secret_field", true, MAX_NAME)
        .and_then(|name| {
            if is_token(name) {
                Some(name.to_owned())
            } else {
                walk.add(code::INVALID_VALUE, secret_at.clone());
                None
            }
        });
    walk.finish(obj);
    Some(VerificationSpec {
        signature_header: header?,
        signature_prefix: prefix?,
        timestamp_header: timestamp,
        tolerance_seconds: tolerance.unwrap_or(DEFAULT_TOLERANCE_SECONDS),
        secret_field: secret_field?,
    })
}

fn header_name(
    walk: &mut Walk,
    obj: &mut Obj<'_>,
    key: &'static str,
    required: bool,
) -> Option<String> {
    let at = obj.at(key);
    let name = walk.text(obj, key, required, MAX_NAME)?;
    if zirv_flow_core::signature::check_header_name(name) {
        Some(name.to_owned())
    } else {
        walk.add(code::INVALID_HEADER, at);
        None
    }
}

/// What only the whole manifest can check: a destination reads no OAuth2 grant, and its credential
/// schema holds the feedback signing secret as a secret field.
pub(super) fn cross_check(
    walk: &mut Walk,
    provider: &Provider,
    headers: &[(String, String)],
    auth: Option<&Auth>,
    credential: Option<&CredentialSpec>,
) {
    if matches!(
        auth,
        Some(Auth::OAuth2ClientCredentials { .. } | Auth::OAuth2AuthorizationCode { .. })
    ) {
        walk.add(code::NOT_FOR_DESTINATIONS, "/auth/type".to_owned());
    }
    // A static header of the idempotency header's name would give the request two values or a
    // constant one.
    for (name, _) in headers {
        if name.eq_ignore_ascii_case(&provider.send.idempotency_header) {
            walk.add(code::FORBIDDEN_HEADER, format!("/headers/{name}"));
        }
    }
    // The same goes for an api_key auth header: it is applied after the idempotency header and
    // would replace the message id with the credential.
    if let Some(Auth::ApiKey {
        location: KeyLocation::Header,
        name,
    }) = auth
        && name.eq_ignore_ascii_case(&provider.send.idempotency_header)
    {
        walk.add(code::FORBIDDEN_HEADER, "/auth/name".to_owned());
    }
    let secret_at = "/destination/feedback/verification/secret_field".to_owned();
    match credential.map(CredentialSpec::schema) {
        None => walk.add(code::REQUIRED, "/auth/credential".to_owned()),
        Some(schema) => match schema.field(&provider.feedback.verification.secret_field) {
            None => walk.add(code::CREDENTIAL_FIELD_MISSING, secret_at),
            Some(field) if !field.secret => walk.add(code::CREDENTIAL_FIELD_NOT_SECRET, secret_at),
            Some(_) => {}
        },
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::{code, validate};
    use super::*;

    fn manifest() -> Value {
        json!({
            "version": "v1",
            "base_url": "https://api.mailer.example.test/v1",
            "auth": {
                "type": "bearer",
                "credential": {"fields": [
                    {"key": "token", "label": "Token", "type": "string", "required": true, "secret": true},
                    {"key": "feedback_secret", "label": "Feedback secret", "type": "string", "required": true, "secret": true}
                ]}
            },
            "destination": {
                "send": {
                    "method": "POST",
                    "path": "/messages",
                    "idempotency_header": "Idempotency-Key",
                    "body": {
                        "fields": {
                            "from": "/sender/address",
                            "from_name": "/sender/name",
                            "to": "/recipients/0",
                            "subject": "/subject",
                            "html": "/html",
                            "message_id": "/meta/message_id"
                        },
                        "static": {"/stream": "outbound", "/track": false}
                    }
                },
                "rate_limit": {"requests": 20, "per_seconds": 1},
                "errors": [
                    {"status": 422, "body": {"pointer": "/code", "values": ["bad_recipient"]}, "action": "recipient_failure"},
                    {"status": "500-503", "action": "retry"}
                ],
                "feedback": {
                    "event_type": "/event",
                    "events": {"delivered": ["delivered"], "bounced": ["bounce"], "complained": ["complaint"]},
                    "message_id": "/meta/message_id",
                    "bounce_kind": {"pointer": "/kind", "hard": ["permanent"], "soft": ["transient"]},
                    "event_id": "/id",
                    "verification": {
                        "signature_header": "X-Mailer-Signature",
                        "signature_prefix": "sha256=",
                        "timestamp_header": "X-Mailer-Timestamp",
                        "tolerance_seconds": 120,
                        "secret_field": "feedback_secret"
                    }
                }
            }
        })
    }

    fn issues(value: &Value) -> Vec<(&'static str, String)> {
        validate(value)
            .err()
            .unwrap_or_default()
            .into_iter()
            .map(|issue| (issue.code, issue.path))
            .collect()
    }

    fn has(value: &Value, code: &'static str, path: &str) -> bool {
        issues(value).contains(&(code, path.to_owned()))
    }

    #[test]
    fn a_provider_manifest_is_valid_with_no_streams_and_reads_back() {
        let parsed = validate(&manifest()).unwrap();
        let provider = parsed.provider.unwrap();

        assert!(parsed.streams.is_empty());
        assert_eq!(provider.send.path, "/messages");
        assert_eq!(provider.send.idempotency_header, "Idempotency-Key");
        assert_eq!(provider.send.fields.len(), 6);
        assert_eq!(provider.send.statics.len(), 2);
        assert_eq!(provider.rate_limit.unwrap().requests, 20);
        assert_eq!(
            provider.errors[1].status,
            StatusRange { min: 500, max: 503 }
        );
        assert_eq!(provider.errors[0].action, ErrorAction::RecipientFailure);
        assert_eq!(provider.feedback.verification.tolerance_seconds, 120);
        assert_eq!(
            provider.feedback.verification.timestamp_header.as_deref(),
            Some("X-Mailer-Timestamp")
        );
    }

    #[test]
    fn both_fixture_provider_manifests_are_valid_and_differ_in_what_they_declare() {
        let one: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/providers/bearer_timestamped.json"
        ))
        .unwrap();
        let two: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/providers/api_key_body_only.json"
        ))
        .unwrap();

        let one = validate(&one).unwrap().provider.unwrap();
        let two = validate(&two).unwrap().provider.unwrap();

        assert!(one.domain_status.is_some() && two.domain_status.is_none());
        assert!(one.rate_limit.is_some() && two.rate_limit.is_none());
        assert!(one.feedback.verification.timestamp_header.is_some());
        assert!(two.feedback.verification.timestamp_header.is_none());
        assert_eq!(two.feedback.verification.signature_prefix, "");
        assert_eq!(two.send.method, SendMethod::Put);
        assert_eq!(two.feedback.bounce_kind.unwrap().hard, ["hard", "5"]);
    }

    #[test]
    fn a_manifest_without_a_destination_still_needs_streams() {
        let mut value = manifest();
        value.as_object_mut().unwrap().remove("destination");

        assert!(has(&value, code::REQUIRED, "/streams"));
    }

    #[test]
    fn feedback_without_verification_is_refused_with_its_pointer() {
        let mut value = manifest();
        value["destination"]["feedback"]
            .as_object_mut()
            .unwrap()
            .remove("verification");

        assert!(has(
            &value,
            code::REQUIRED,
            "/destination/feedback/verification"
        ));
        assert!(validate(&value).is_err());
    }

    #[test]
    fn an_unknown_key_and_a_script_key_are_refused_at_every_level() {
        let mut value = manifest();
        value["destination"]["zzz"] = json!(1);
        value["destination"]["send"]["body"]["fields"]["cc"] = json!("/cc");
        value["destination"]["feedback"]["verification"]["script"] = json!("x");
        value["destination"]["errors"][0]["extra"] = json!(true);

        assert!(has(&value, code::UNKNOWN_FIELD, "/destination/zzz"));
        assert!(has(
            &value,
            code::UNKNOWN_FIELD,
            "/destination/send/body/fields/cc"
        ));
        assert!(has(
            &value,
            code::EXPRESSION_NOT_SUPPORTED,
            "/destination/feedback/verification/script"
        ));
        assert!(has(
            &value,
            code::UNKNOWN_FIELD,
            "/destination/errors/0/extra"
        ));
    }

    #[test]
    fn a_template_marker_in_the_destination_is_refused() {
        let mut value = manifest();
        value["destination"]["send"]["body"]["static"]["/stream"] = json!("{{ message.id }}");
        value["destination"]["send"]["path"] = json!("/m/${id}");

        assert!(has(
            &value,
            code::EXPRESSION_NOT_SUPPORTED,
            "/destination/send/body/static/~1stream"
        ));
        assert!(has(
            &value,
            code::EXPRESSION_NOT_SUPPORTED,
            "/destination/send/path"
        ));
    }

    #[test]
    fn a_body_field_outside_the_closed_set_or_a_missing_required_one_is_refused() {
        let mut value = manifest();
        value["destination"]["send"]["body"]["fields"]
            .as_object_mut()
            .unwrap()
            .remove("subject");
        value["destination"]["send"]["body"]["fields"]["bcc"] = json!("/bcc");

        assert!(has(
            &value,
            code::REQUIRED,
            "/destination/send/body/fields/subject"
        ));
        assert!(has(
            &value,
            code::UNKNOWN_FIELD,
            "/destination/send/body/fields/bcc"
        ));
    }

    #[test]
    fn a_body_needs_html_or_text() {
        let mut value = manifest();
        value["destination"]["send"]["body"]["fields"]
            .as_object_mut()
            .unwrap()
            .remove("html");

        assert!(has(
            &value,
            code::REQUIRED,
            "/destination/send/body/fields/html"
        ));
        value["destination"]["send"]["body"]["fields"]["text"] = json!("/text");
        assert!(validate(&value).is_ok());
    }

    #[test]
    fn colliding_placements_and_bad_pointers_are_refused() {
        let mut value = manifest();
        value["destination"]["send"]["body"]["static"]["/sender"] = json!("x");
        assert!(has(
            &value,
            code::POINTER_CONFLICT,
            "/destination/send/body"
        ));

        let mut value = manifest();
        value["destination"]["send"]["body"]["fields"]["to"] = json!("/recipients/3");
        assert!(has(
            &value,
            code::POINTER_CONFLICT,
            "/destination/send/body"
        ));

        for bad in ["", "no-slash", "/a//b", "/a/~2", "/a/99"] {
            let mut value = manifest();
            value["destination"]["send"]["body"]["fields"]["subject"] = json!(bad);
            assert!(
                has(
                    &value,
                    code::INVALID_POINTER,
                    "/destination/send/body/fields/subject"
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_idempotency_header_is_a_token_the_runtime_does_not_own() {
        let mut value = manifest();
        value["destination"]["send"]["idempotency_header"] = json!("Authorization");
        assert!(has(
            &value,
            code::FORBIDDEN_HEADER,
            "/destination/send/idempotency_header"
        ));
        value["destination"]["send"]["idempotency_header"] = json!("Bad Header");
        assert!(has(
            &value,
            code::INVALID_HEADER,
            "/destination/send/idempotency_header"
        ));
    }

    #[test]
    fn an_api_key_header_named_like_the_idempotency_header_is_refused_ignoring_case() {
        let mut value = manifest();
        value["auth"] = json!({
            "type": "api_key", "in": "header", "name": "idempotency-KEY",
            "credential": {"fields": [
                {"key": "api_key", "label": "Key", "type": "string", "required": true, "secret": true},
                {"key": "feedback_secret", "label": "Feedback secret", "type": "string", "required": true, "secret": true}
            ]}
        });
        assert!(has(&value, code::FORBIDDEN_HEADER, "/auth/name"));

        value["auth"]["in"] = json!("query");
        assert!(!has(&value, code::FORBIDDEN_HEADER, "/auth/name"));
    }

    #[test]
    fn error_rules_take_a_status_or_a_range_within_400_to_599_and_a_known_action() {
        for status in [
            json!(200),
            json!("300-399"),
            json!("503-500"),
            json!(true),
            json!("x"),
        ] {
            let mut value = manifest();
            value["destination"]["errors"][1]["status"] = status;
            assert!(
                !issues(&value).is_empty(),
                "{:?}",
                value["destination"]["errors"][1]
            );
        }
        let mut value = manifest();
        value["destination"]["errors"][0]["action"] = json!("explode");
        assert!(has(
            &value,
            code::INVALID_VALUE,
            "/destination/errors/0/action"
        ));
    }

    #[test]
    fn a_destination_manifest_refuses_oauth_and_needs_its_secret_as_a_secret_credential_field() {
        let mut value = manifest();
        value["auth"] = json!({
            "type": "oauth2_client_credentials",
            "token_url": "https://api.mailer.example.test/token"
        });
        assert!(has(&value, code::NOT_FOR_DESTINATIONS, "/auth/type"));

        let mut value = manifest();
        value["destination"]["feedback"]["verification"]["secret_field"] = json!("nonesuch");
        assert!(has(
            &value,
            code::CREDENTIAL_FIELD_MISSING,
            "/destination/feedback/verification/secret_field"
        ));

        let mut value = manifest();
        value["auth"]["credential"]["fields"][1]["secret"] = json!(false);
        assert!(has(
            &value,
            code::CREDENTIAL_FIELD_NOT_SECRET,
            "/destination/feedback/verification/secret_field"
        ));

        let mut value = manifest();
        value["auth"].as_object_mut().unwrap().remove("credential");
        assert!(has(&value, code::REQUIRED, "/auth/credential"));
    }

    #[test]
    fn feedback_needs_a_bounce_kind_when_it_maps_bounces() {
        let mut value = manifest();
        value["destination"]["feedback"]
            .as_object_mut()
            .unwrap()
            .remove("bounce_kind");

        assert!(has(
            &value,
            code::REQUIRED,
            "/destination/feedback/bounce_kind"
        ));
    }

    #[test]
    fn the_domain_status_request_and_its_mapping_are_validated() {
        let mut value = manifest();
        value["destination"]["domain_status"] = json!({
            "path": "/domains",
            "domain_param": "domain",
            "records_path": "/records",
            "name": "/name",
            "type": "/type",
            "status": "/state",
            "status_map": {"verified": ["ok"], "pending": ["waiting"], "failed": ["bad"]}
        });
        let domain = validate(&value)
            .unwrap()
            .provider
            .unwrap()
            .domain_status
            .unwrap();
        assert_eq!(domain.verified, ["ok"]);
        assert_eq!(domain.records_path, "/records");

        value["destination"]["domain_status"]["status_map"] = json!({});
        assert!(has(
            &value,
            code::REQUIRED,
            "/destination/domain_status/status_map"
        ));
        value["destination"]["domain_status"]["status_map"] = json!({"verified": ["ok"], "x": 1});
        assert!(has(
            &value,
            code::UNKNOWN_FIELD,
            "/destination/domain_status/status_map/x"
        ));
    }

    #[test]
    fn a_placement_builds_objects_and_arrays_and_refuses_a_collision_or_a_gap() {
        let mut body = Value::Null;
        place(&mut body, "/a/b", json!(1)).unwrap();
        place(&mut body, "/list/0", json!("x")).unwrap();
        place(&mut body, "/list/1", json!("y")).unwrap();
        place(&mut body, "/we~1ird/k~0", json!(true)).unwrap();

        assert_eq!(
            body,
            json!({"a": {"b": 1}, "list": ["x", "y"], "we/ird": {"k~": true}})
        );
        assert_eq!(place(&mut body, "/a/b", json!(2)), Err(PlaceError));
        assert_eq!(place(&mut body, "/a/b/c", json!(2)), Err(PlaceError));
        assert_eq!(place(&mut body, "/list/5", json!(2)), Err(PlaceError));
        assert_eq!(place(&mut body, "/a/0", json!(2)), Err(PlaceError));
    }

    #[test]
    fn no_secret_value_or_literal_is_ever_copied_into_an_issue() {
        let mut value = manifest();
        value["destination"]["send"]["body"]["static"]["/secret-value-xyz"] = json!({});
        value["destination"]["feedback"]["verification"]["signature_header"] =
            json!("secret value xyz");

        for issue in validate(&value).unwrap_err() {
            assert!(!issue.to_string().contains("secret value"), "{issue}");
        }
    }
}
