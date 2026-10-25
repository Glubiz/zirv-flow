//! The profile operations of a provider manifest's `destination` section (architecture 10.2,
//! "As built (profile sync, #226)"): `upsert_profile` and `remove_profile`, how `action.sync`
//! keeps a person's profile in an external system. Like the rest of the manifest they are data
//! only: a closed set of keys, JSON pointers and literal values, with no expression language.
//!
//! An operation is one HTTP request:
//!
//! - `method` and `path`. The path may hold exactly one placeholder, `{key}`, replaced at run time
//!   by the external key, percent-encoded as one path segment. No other placeholder exists, and a
//!   `{{ }}` marker is refused by the manifest-wide scan.
//! - `idempotency_header`, which carries the node's effect key, so a retry cannot duplicate.
//! - `key_from`, where the external key comes from: `{"identity": "<identity kind>"}` or
//!   `{"trait": "<trait key>"}`.
//! - `body`: `key` places the external key at a JSON pointer; `traits` places one object holding
//!   the node's listed traits at a pointer, or `fields` places single traits at pointers (an
//!   upsert has one of the two, a remove neither); `static` places literals beside them.
//! - `rate_limit` and `errors`, in the format of the email send (#223).
//!
//! This module is the model and the validator. The runtime is `destinations::provider`.

use serde_json::Value;
use zirv_flow_core::people::{IdentifierKind, validate_key};

use super::destination::{
    ErrorAction, ErrorRule, MAX_ERROR_RULES, error_rule, idempotency_header, place, statics,
    valid_placement,
};
use super::issue::{child, code, index};
use super::model::RateLimitSpec;
use super::parse::{MAX_NAME, Obj, Walk, rate_limit, stream_path};

/// The most traits one profile operation places by `fields`, and so the most a node may list.
pub const MAX_PROFILE_TRAITS: usize = 50;
/// The placeholder a path may hold once.
pub const KEY_PLACEHOLDER: &str = "{key}";

/// The HTTP method of a profile operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProfileMethod {
    Post,
    Put,
    Patch,
    Delete,
}

/// Where the external key comes from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeySource {
    /// The person's identity of this kind (`email`, `phone`, `external:<namespace>`).
    Identity(String),
    /// The person's value of this trait.
    Trait(String),
}

/// How the node's listed traits are placed in the body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TraitPlacement {
    /// All of them, as one object, at this pointer.
    Object(String),
    /// Each at its own pointer: `(trait key, pointer)`. A listed trait the manifest does not name
    /// is not sent.
    Fields(Vec<(String, String)>),
}

/// One profile operation of a validated manifest.
#[derive(Clone, Debug)]
pub struct ProfileSpec {
    pub method: ProfileMethod,
    /// Starts with `/`, relative to the base URL's path; holds [`KEY_PLACEHOLDER`] at most once.
    pub path: String,
    /// The header that carries the node's effect key as the idempotency key.
    pub idempotency_header: String,
    pub key_from: KeySource,
    /// Where the external key is placed in the body, when it is.
    pub key: Option<String>,
    pub traits: Option<TraitPlacement>,
    /// Literal values placed in the body beside the rest, by JSON pointer.
    pub statics: Vec<(String, Value)>,
    pub rate_limit: Option<RateLimitSpec>,
    /// Checked before the destination's own rules; the first rule that matches wins.
    pub errors: Vec<ErrorRule>,
}

impl ProfileSpec {
    /// Whether the request has a body: anything is placed in it.
    pub fn has_body(&self) -> bool {
        self.key.is_some() || self.traits.is_some() || !self.statics.is_empty()
    }
}

/// How many times the placeholder occurs in `path`, and whether the path holds any other brace.
fn placeholders(path: &str) -> (usize, bool) {
    let count = path.matches(KEY_PLACEHOLDER).count();
    let rest = path.replace(KEY_PLACEHOLDER, "");
    (count, rest.contains(['{', '}']))
}

pub(super) fn profile(
    walk: &mut Walk,
    value: &Value,
    path: String,
    remove: bool,
) -> Option<ProfileSpec> {
    let mut obj = walk.object(value, path)?;
    let methods: &[&str] = if remove {
        &["DELETE", "POST", "PUT", "PATCH"]
    } else {
        &["POST", "PUT", "PATCH"]
    };
    let method = match walk.choice(&mut obj, "method", false, methods) {
        Some("POST") => ProfileMethod::Post,
        Some("PUT") => ProfileMethod::Put,
        Some("PATCH") => ProfileMethod::Patch,
        Some(_) => ProfileMethod::Delete,
        None if remove => ProfileMethod::Delete,
        None => ProfileMethod::Post,
    };
    let request_path = stream_path(walk, &mut obj).filter(|text| {
        let (count, stray) = placeholders(text);
        let ok = count <= 1 && !stray;
        if !ok {
            walk.add(code::PATH_INVALID, obj.at("path"));
        }
        ok
    });
    let header = idempotency_header(walk, &mut obj);
    let key_from = match obj.take("key_from") {
        Some(value) => key_from(walk, value, obj.at("key_from")),
        None => {
            walk.missing(&obj, "key_from");
            None
        }
    };
    let in_path = request_path
        .as_deref()
        .is_some_and(|text| placeholders(text).0 == 1);
    let body_at = obj.at("body");
    let body = obj
        .take("body")
        .and_then(|value| body(walk, value, body_at.clone(), remove));
    let rate_limit = obj
        .take("rate_limit")
        .and_then(|value| rate_limit(walk, value, obj.at("rate_limit")));
    let errors = obj
        .take("errors")
        .map(|value| errors(walk, value, obj.at("errors")))
        .unwrap_or_default();
    walk.finish(obj);

    let body = body.unwrap_or_default();
    // The key goes somewhere: in the path, in the body, or both.
    if !in_path && body.key.is_none() {
        walk.add(code::REQUIRED, child(&body_at, "key"));
    }
    if !remove && body.traits.is_none() {
        walk.add(code::REQUIRED, child(&body_at, "traits"));
    }
    Some(ProfileSpec {
        method,
        path: request_path?,
        idempotency_header: header?,
        key_from: key_from?,
        key: body.key,
        traits: body.traits,
        statics: body.statics,
        rate_limit,
        errors,
    })
}

fn key_from(walk: &mut Walk, value: &Value, path: String) -> Option<KeySource> {
    let mut obj = walk.object(value, path)?;
    let identity_at = obj.at("identity");
    let identity = walk.text(&mut obj, "identity", false, MAX_NAME);
    let trait_at = obj.at("trait");
    let trait_key = walk.text(&mut obj, "trait", false, MAX_NAME);
    let parent = obj.path.clone();
    walk.finish(obj);
    match (identity, trait_key) {
        (Some(kind), None) => {
            if IdentifierKind::parse(kind).is_ok() {
                Some(KeySource::Identity(kind.to_owned()))
            } else {
                walk.add(code::INVALID_VALUE, identity_at);
                None
            }
        }
        (None, Some(key)) => {
            if validate_key(key).is_ok() {
                Some(KeySource::Trait(key.to_owned()))
            } else {
                walk.add(code::INVALID_VALUE, trait_at);
                None
            }
        }
        // Neither, or both: one place the key comes from.
        (None, None) => {
            // A key that failed on its own already has its issue.
            if !reported_under(&parent, walk) {
                walk.add(code::REQUIRED, child(&parent, "identity"));
            }
            None
        }
        (Some(_), Some(_)) => {
            walk.add(code::POINTER_CONFLICT, parent);
            None
        }
    }
}

/// Whether an issue already names a key under `parent`: a `key_from` whose one key failed on its
/// own (a wrong type or length) is not also reported as missing.
fn reported_under(parent: &str, walk: &Walk) -> bool {
    walk.issues
        .iter()
        .any(|issue| issue.path.starts_with(&format!("{parent}/")))
}

/// What a profile operation's `body` places.
#[derive(Default)]
struct Body {
    key: Option<String>,
    traits: Option<TraitPlacement>,
    statics: Vec<(String, Value)>,
}

fn body(walk: &mut Walk, value: &Value, path: String, remove: bool) -> Option<Body> {
    let mut obj = walk.object(value, path.clone())?;
    let key_at = obj.at("key");
    let key = match walk.pointer(&mut obj, "key", false) {
        Some(pointer) if valid_placement(pointer) => Some(pointer.to_owned()),
        Some(_) => {
            walk.add(code::INVALID_POINTER, key_at);
            None
        }
        None => None,
    };
    // A remove names no trait: its keys are unknown to it.
    let traits = if remove { None } else { traits(walk, &mut obj) };
    let statics = obj
        .take("static")
        .map(|value| statics(walk, value, obj.at("static")))
        .unwrap_or_default();
    walk.finish(obj);

    // A dry run with placeholders finds two placements that collide.
    let mut probe = Value::Null;
    let mut pointers: Vec<&str> = key.iter().map(String::as_str).collect();
    match &traits {
        Some(TraitPlacement::Object(pointer)) => pointers.push(pointer),
        Some(TraitPlacement::Fields(fields)) => {
            pointers.extend(fields.iter().map(|(_, pointer)| pointer.as_str()));
        }
        None => {}
    }
    pointers.extend(statics.iter().map(|(pointer, _)| pointer.as_str()));
    let mut collides = false;
    for pointer in pointers {
        collides |= place(&mut probe, pointer, Value::Bool(true)).is_err();
    }
    if collides {
        walk.add(code::POINTER_CONFLICT, path);
        return None;
    }
    Some(Body {
        key,
        traits,
        statics,
    })
}

/// `traits` (one object) or `fields` (single traits), not both.
fn traits(walk: &mut Walk, obj: &mut Obj<'_>) -> Option<TraitPlacement> {
    let object_at = obj.at("traits");
    let object = match walk.pointer(obj, "traits", false) {
        Some(pointer) if valid_placement(pointer) => Some(pointer.to_owned()),
        Some(_) => {
            walk.add(code::INVALID_POINTER, object_at);
            None
        }
        None => None,
    };
    let fields_at = obj.at("fields");
    let fields = obj
        .take("fields")
        .and_then(|value| fields(walk, value, fields_at.clone()));
    match (object, fields) {
        (Some(pointer), None) => Some(TraitPlacement::Object(pointer)),
        (None, Some(fields)) => Some(TraitPlacement::Fields(fields)),
        (Some(_), Some(_)) => {
            walk.add(code::POINTER_CONFLICT, fields_at);
            None
        }
        (None, None) => None,
    }
}

fn fields(walk: &mut Walk, value: &Value, path: String) -> Option<Vec<(String, String)>> {
    let Some(map) = value.as_object() else {
        walk.add(code::INVALID_TYPE, path);
        return None;
    };
    if map.is_empty() || map.len() > MAX_PROFILE_TRAITS {
        walk.add(code::OUT_OF_RANGE, path);
        return None;
    }
    let mut fields = Vec::new();
    for (key, pointer) in map {
        let at = child(&path, key);
        if validate_key(key).is_err() {
            walk.add(code::INVALID_FIELD_NAME, at);
            continue;
        }
        match pointer.as_str() {
            Some(text) if valid_placement(text) => fields.push((key.clone(), text.to_owned())),
            Some(_) => walk.add(code::INVALID_POINTER, at),
            None => walk.add(code::INVALID_TYPE, at),
        }
    }
    Some(fields)
}

/// The error rules of an operation. A recipient failure is about an address that cannot receive
/// mail, which a profile operation has no notion of.
fn errors(walk: &mut Walk, value: &Value, path: String) -> Vec<ErrorRule> {
    let Some(items) = value.as_array() else {
        walk.add(code::INVALID_TYPE, path);
        return Vec::new();
    };
    if items.len() > MAX_ERROR_RULES {
        walk.add(code::OUT_OF_RANGE, path);
        return Vec::new();
    }
    let mut rules = Vec::new();
    for (position, item) in items.iter().enumerate() {
        let at = index(&path, position);
        if let Some(rule) = error_rule(walk, item, at.clone()) {
            if rule.action == ErrorAction::RecipientFailure {
                walk.add(code::INVALID_VALUE, child(&at, "action"));
            } else {
                rules.push(rule);
            }
        }
    }
    rules
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::{code, validate};
    use super::*;

    fn manifest() -> Value {
        json!({
            "version": "v1",
            "base_url": "https://api.profiles.example.test/v2",
            "auth": {"type": "bearer"},
            "destination": {
                "upsert_profile": {
                    "method": "PUT",
                    "path": "/profiles/{key}",
                    "idempotency_header": "Idempotency-Key",
                    "key_from": {"identity": "email"},
                    "body": {
                        "key": "/profile/id",
                        "traits": "/profile/attributes",
                        "static": {"/source": "zirv"}
                    },
                    "rate_limit": {"requests": 5, "per_seconds": 1},
                    "errors": [{"status": 409, "action": "retry"}]
                },
                "remove_profile": {
                    "path": "/profiles/{key}",
                    "idempotency_header": "Idempotency-Key",
                    "key_from": {"identity": "email"}
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
    fn a_manifest_with_only_profile_operations_is_valid_and_reads_back() {
        let parsed = validate(&manifest()).unwrap();
        let provider = parsed.provider.unwrap();

        assert!(provider.send.is_none() && provider.feedback.is_none());
        let upsert = provider.upsert_profile.unwrap();
        assert_eq!(upsert.method, ProfileMethod::Put);
        assert_eq!(upsert.path, "/profiles/{key}");
        assert_eq!(upsert.key_from, KeySource::Identity("email".to_owned()));
        assert_eq!(upsert.key.as_deref(), Some("/profile/id"));
        assert_eq!(
            upsert.traits,
            Some(TraitPlacement::Object("/profile/attributes".to_owned()))
        );
        assert_eq!(upsert.statics.len(), 1);
        assert_eq!(upsert.rate_limit.unwrap().requests, 5);
        assert_eq!(upsert.errors.len(), 1);
        let remove = provider.remove_profile.unwrap();
        assert_eq!(remove.method, ProfileMethod::Delete, "the default");
        assert!(!remove.has_body());
    }

    #[test]
    fn a_manifest_may_declare_one_operation_and_the_methods_default_by_operation() {
        let mut value = manifest();
        value["destination"]
            .as_object_mut()
            .unwrap()
            .remove("remove_profile");
        value["destination"]["upsert_profile"]
            .as_object_mut()
            .unwrap()
            .remove("method");

        let provider = validate(&value).unwrap().provider.unwrap();

        assert!(provider.remove_profile.is_none());
        assert_eq!(provider.upsert_profile.unwrap().method, ProfileMethod::Post);
    }

    #[test]
    fn a_destination_with_nothing_to_do_is_refused_by_pointer() {
        let mut value = manifest();
        value["destination"] = json!({});

        assert!(has(&value, code::REQUIRED, "/destination/send"));
        assert!(has(&value, code::REQUIRED, "/destination/feedback"));
    }

    #[test]
    fn the_email_half_is_a_pair() {
        let email = serde_json::from_str::<Value>(include_str!(
            "../../tests/fixtures/providers/bearer_timestamped.json"
        ))
        .unwrap();
        let mut send_only = email.clone();
        send_only["destination"]
            .as_object_mut()
            .unwrap()
            .remove("feedback");
        let mut feedback_only = email.clone();
        feedback_only["destination"]
            .as_object_mut()
            .unwrap()
            .remove("send");
        // With a profile operation beside it, a half is still a half.
        let mut with_profile = send_only.clone();
        with_profile["destination"]["remove_profile"] =
            manifest()["destination"]["remove_profile"].clone();

        assert!(has(&send_only, code::REQUIRED, "/destination/feedback"));
        assert!(has(&feedback_only, code::REQUIRED, "/destination/send"));
        assert!(has(&with_profile, code::REQUIRED, "/destination/feedback"));
        assert!(validate(&email).is_ok());
    }

    #[test]
    fn email_and_profile_operations_can_share_a_manifest() {
        let mut value = serde_json::from_str::<Value>(include_str!(
            "../../tests/fixtures/providers/bearer_timestamped.json"
        ))
        .unwrap();
        value["destination"]["upsert_profile"] =
            manifest()["destination"]["upsert_profile"].clone();

        let provider = validate(&value).unwrap().provider.unwrap();

        assert!(provider.send.is_some() && provider.feedback.is_some());
        assert!(provider.upsert_profile.is_some() && provider.remove_profile.is_none());
    }

    #[test]
    fn an_unknown_key_is_refused_by_pointer_at_every_level() {
        let mut value = manifest();
        value["destination"]["upsert_profile"]["zzz"] = json!(1);
        value["destination"]["upsert_profile"]["body"]["extra"] = json!("/x");
        value["destination"]["upsert_profile"]["key_from"]["other"] = json!("x");
        value["destination"]["remove_profile"]["errors"] =
            json!([{"status": 500, "action": "retry", "why": 1}]);
        // A remove names no trait.
        value["destination"]["remove_profile"]["body"] = json!({"key": "/id", "traits": "/t"});

        assert!(has(
            &value,
            code::UNKNOWN_FIELD,
            "/destination/upsert_profile/zzz"
        ));
        assert!(has(
            &value,
            code::UNKNOWN_FIELD,
            "/destination/upsert_profile/body/extra"
        ));
        assert!(has(
            &value,
            code::UNKNOWN_FIELD,
            "/destination/upsert_profile/key_from/other"
        ));
        assert!(has(
            &value,
            code::UNKNOWN_FIELD,
            "/destination/remove_profile/errors/0/why"
        ));
        assert!(has(
            &value,
            code::UNKNOWN_FIELD,
            "/destination/remove_profile/body/traits"
        ));
    }

    #[test]
    fn a_script_key_is_not_an_expression() {
        let mut value = manifest();
        value["destination"]["upsert_profile"]["body"]["script"] = json!("x");

        assert!(has(
            &value,
            code::EXPRESSION_NOT_SUPPORTED,
            "/destination/upsert_profile/body/script"
        ));
    }

    #[test]
    fn a_path_holds_the_key_placeholder_at_most_once_and_no_other_placeholder() {
        for (path, ok) in [
            ("/profiles/{key}", true),
            ("/profiles", true),
            ("/a/{key}/b", true),
            ("/profiles/{key}/{key}", false),
            ("/profiles/{id}", false),
            ("/profiles/{key", false),
            ("/profiles/key}", false),
            ("/profiles/{key}{", false),
        ] {
            let mut value = manifest();
            value["destination"]["upsert_profile"]["path"] = json!(path);
            // A path with no placeholder needs the key in the body, which the fixture has.

            assert_eq!(validate(&value).is_ok(), ok, "{path}: {:?}", issues(&value));
            if !ok {
                assert!(
                    has(
                        &value,
                        code::PATH_INVALID,
                        "/destination/upsert_profile/path"
                    ),
                    "{path}"
                );
            }
        }
    }

    #[test]
    fn a_template_marker_in_a_path_or_a_static_is_refused() {
        for (at, value) in [
            ("path", json!("/profiles/{{ key }}")),
            ("path", json!("/p/${key}")),
        ] {
            let mut manifest = manifest();
            manifest["destination"]["upsert_profile"][at] = value;

            assert!(
                validate(&manifest)
                    .err()
                    .unwrap_or_default()
                    .iter()
                    .any(|issue| issue.code == code::EXPRESSION_NOT_SUPPORTED),
                "{at}"
            );
        }
        let mut manifest = manifest();
        manifest["destination"]["upsert_profile"]["body"]["static"]["/source"] = json!("{{ x }}");

        assert!(has(
            &manifest,
            code::EXPRESSION_NOT_SUPPORTED,
            "/destination/upsert_profile/body/static/~1source"
        ));
    }

    #[test]
    fn key_from_names_exactly_one_known_source() {
        let at = "/destination/upsert_profile/key_from";
        for (value, expected) in [
            (json!({"identity": "phone"}), None),
            (json!({"identity": "external:crm"}), None),
            (json!({"trait": "plan"}), None),
            (json!({}), Some((code::REQUIRED, format!("{at}/identity")))),
            (
                json!({"identity": "email", "trait": "plan"}),
                Some((code::POINTER_CONFLICT, at.to_owned())),
            ),
            (
                json!({"identity": "Email"}),
                Some((code::INVALID_VALUE, format!("{at}/identity"))),
            ),
            (
                json!({"trait": "Not A Key"}),
                Some((code::INVALID_VALUE, format!("{at}/trait"))),
            ),
            (
                json!({"identity": 5}),
                Some((code::INVALID_TYPE, format!("{at}/identity"))),
            ),
        ] {
            let mut value_of = manifest();
            value_of["destination"]["upsert_profile"]["key_from"] = value.clone();
            let found = issues(&value_of);

            match expected {
                None => assert!(found.is_empty(), "{value}: {found:?}"),
                Some((code, path)) => assert!(found.contains(&(code, path)), "{value}: {found:?}"),
            }
        }
    }

    #[test]
    fn a_missing_key_from_is_refused_with_its_pointer() {
        let mut value = manifest();
        value["destination"]["remove_profile"]
            .as_object_mut()
            .unwrap()
            .remove("key_from");

        assert!(has(
            &value,
            code::REQUIRED,
            "/destination/remove_profile/key_from"
        ));
    }

    #[test]
    fn the_key_goes_in_the_path_or_the_body() {
        let mut value = manifest();
        value["destination"]["remove_profile"]["path"] = json!("/profiles");

        assert!(has(
            &value,
            code::REQUIRED,
            "/destination/remove_profile/body/key"
        ));
        value["destination"]["remove_profile"]["body"] = json!({"key": "/id"});
        assert!(validate(&value).is_ok());
    }

    #[test]
    fn an_upsert_places_its_traits_by_one_pointer_or_by_fields_and_not_both() {
        let mut value = manifest();
        value["destination"]["upsert_profile"]["body"]["fields"] = json!({"plan": "/p"});

        assert!(has(
            &value,
            code::POINTER_CONFLICT,
            "/destination/upsert_profile/body/fields"
        ));

        let mut fields = manifest();
        let body = fields["destination"]["upsert_profile"]["body"]
            .as_object_mut()
            .unwrap();
        body.remove("traits");
        body.insert(
            "fields".to_owned(),
            json!({"plan": "/p", "country": "/c/name"}),
        );
        let spec = validate(&fields)
            .unwrap()
            .provider
            .unwrap()
            .upsert_profile
            .unwrap();
        let Some(TraitPlacement::Fields(placed)) = spec.traits else {
            panic!("fields");
        };
        assert_eq!(placed.len(), 2);

        let mut neither = manifest();
        neither["destination"]["upsert_profile"]["body"]
            .as_object_mut()
            .unwrap()
            .remove("traits");
        assert!(has(
            &neither,
            code::REQUIRED,
            "/destination/upsert_profile/body/traits"
        ));
    }

    #[test]
    fn fields_are_trait_keys_at_valid_pointers_within_the_limit() {
        let mut value = manifest();
        let body = value["destination"]["upsert_profile"]["body"]
            .as_object_mut()
            .unwrap();
        body.remove("traits");
        body.insert(
            "fields".to_owned(),
            json!({"Bad Key": "/a", "plan": "no-slash", "age": 3}),
        );

        let at = "/destination/upsert_profile/body/fields";
        assert!(has(
            &value,
            code::INVALID_FIELD_NAME,
            &format!("{at}/Bad Key")
        ));
        assert!(has(&value, code::INVALID_POINTER, &format!("{at}/plan")));
        assert!(has(&value, code::INVALID_TYPE, &format!("{at}/age")));

        let many: serde_json::Map<String, Value> = (0..=MAX_PROFILE_TRAITS)
            .map(|n| (format!("t{n}"), json!(format!("/f/{n}"))))
            .collect();
        value["destination"]["upsert_profile"]["body"]["fields"] = Value::Object(many);
        assert!(has(&value, code::OUT_OF_RANGE, at));
    }

    #[test]
    fn colliding_placements_are_refused() {
        let mut value = manifest();
        value["destination"]["upsert_profile"]["body"]["key"] = json!("/profile");

        assert!(has(
            &value,
            code::POINTER_CONFLICT,
            "/destination/upsert_profile/body"
        ));
    }

    #[test]
    fn methods_are_closed_by_operation_and_a_recipient_failure_is_not_a_profile_error() {
        let mut value = manifest();
        value["destination"]["upsert_profile"]["method"] = json!("DELETE");
        value["destination"]["remove_profile"]["method"] = json!("GET");
        value["destination"]["remove_profile"]["errors"] =
            json!([{"status": 422, "action": "recipient_failure"}]);

        assert!(has(
            &value,
            code::INVALID_VALUE,
            "/destination/upsert_profile/method"
        ));
        assert!(has(
            &value,
            code::INVALID_VALUE,
            "/destination/remove_profile/method"
        ));
        assert!(has(
            &value,
            code::INVALID_VALUE,
            "/destination/remove_profile/errors/0/action"
        ));
    }

    #[test]
    fn the_idempotency_header_is_a_token_the_runtime_does_not_own() {
        let mut value = manifest();
        value["destination"]["upsert_profile"]["idempotency_header"] = json!("Authorization");
        value["destination"]["remove_profile"]["idempotency_header"] = json!("bad header");

        assert!(has(
            &value,
            code::FORBIDDEN_HEADER,
            "/destination/upsert_profile/idempotency_header"
        ));
        assert!(has(
            &value,
            code::INVALID_HEADER,
            "/destination/remove_profile/idempotency_header"
        ));
    }

    #[test]
    fn a_static_or_api_key_header_named_like_an_operations_idempotency_header_is_refused() {
        let mut value = manifest();
        value["headers"] = json!({"idempotency-key": "constant"});
        assert!(has(
            &value,
            code::FORBIDDEN_HEADER,
            "/headers/idempotency-key"
        ));

        let mut keyed = manifest();
        keyed["auth"] = json!({"type": "api_key", "in": "header", "name": "idempotency-KEY"});
        assert!(has(&keyed, code::FORBIDDEN_HEADER, "/auth/name"));
    }

    #[test]
    fn a_profile_only_manifest_needs_no_feedback_secret_and_no_credential_schema() {
        assert!(validate(&manifest()).is_ok());
    }

    #[test]
    fn no_literal_is_copied_into_an_issue() {
        let mut value = manifest();
        value["destination"]["upsert_profile"]["key_from"] = json!({"identity": "SENTINEL-77"});
        value["destination"]["upsert_profile"]["path"] = json!("/p/{SENTINEL-88}");

        let rendered = format!("{:?}", issues(&value));

        assert!(!rendered.contains("SENTINEL"), "{rendered}");
    }
}
