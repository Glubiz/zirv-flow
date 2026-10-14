//! Structural validation of ingestion payloads (architecture section 15): the JSON shape,
//! field types, identifier syntax and name lengths. Whether an event name is planned or its
//! properties fit a schema is decided at drain time, not here.
//!
//! Errors name the item, the field and a fixed reason code, never a value.

use serde_json::{Map, Value};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use zirv_flow_core::events::{RESERVED_EVENT_CODE, is_reserved_event_name, validate_event_name};
use zirv_flow_core::people::Identifier;
use zirv_flow_store::{IngestItem, IngestPayload};

const MAX_MESSAGE_ID_CHARS: usize = 200;
const MAX_IDENTIFIERS: usize = 20;
/// The longest field name an error echoes back.
const MAX_ECHOED_NAME: usize = 64;
/// The most errors one response lists.
const MAX_ERRORS: usize = 100;

/// What one item asks for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Kind {
    Identify,
    Track,
}

/// One invalid field of one item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ItemError {
    pub(crate) index: usize,
    pub(crate) field: String,
    pub(crate) reason: &'static str,
}

fn error(index: usize, field: &str, reason: &'static str) -> ItemError {
    let field: String = field
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_ECHOED_NAME)
        .collect();
    ItemError {
        index,
        field,
        reason,
    }
}

/// The items of a batch body, `{"batch": [...]}`.
pub(crate) fn batch_items(body: &Value, max_items: usize) -> Result<&[Value], Vec<ItemError>> {
    let Some(object) = body.as_object() else {
        return Err(vec![error(0, "batch", "wrong_type")]);
    };
    let mut errors: Vec<ItemError> = object
        .keys()
        .filter(|name| name.as_str() != "batch")
        .map(|name| error(0, name, "unknown_field"))
        .collect();
    let items = match object.get("batch") {
        None => {
            errors.push(error(0, "batch", "required"));
            None
        }
        Some(Value::Array(items)) if items.is_empty() => {
            errors.push(error(0, "batch", "empty"));
            None
        }
        Some(Value::Array(items)) if items.len() > max_items => {
            errors.push(error(0, "batch", "too_many_items"));
            None
        }
        Some(Value::Array(items)) => Some(items),
        Some(_) => {
            errors.push(error(0, "batch", "wrong_type"));
            None
        }
    };
    match items {
        Some(items) if errors.is_empty() => Ok(items),
        _ => Err(errors),
    }
}

/// Validates `values` as items (of `kind`, or each naming its own in `type`) and returns
/// them, or every error found up to a cap.
pub(crate) fn parse_items(
    values: &[Value],
    kind: Option<Kind>,
) -> Result<Vec<IngestItem>, Vec<ItemError>> {
    let mut items = Vec::with_capacity(values.len());
    let mut errors = Vec::new();
    for (index, value) in values.iter().enumerate() {
        match parse_item(index, value, kind) {
            Ok(item) => items.push(item),
            Err(found) => errors.extend(found),
        }
        if errors.len() >= MAX_ERRORS {
            errors.truncate(MAX_ERRORS);
            break;
        }
    }
    if errors.is_empty() {
        Ok(items)
    } else {
        Err(errors)
    }
}

fn parse_item(
    index: usize,
    value: &Value,
    kind: Option<Kind>,
) -> Result<IngestItem, Vec<ItemError>> {
    let Some(object) = value.as_object() else {
        return Err(vec![error(index, "", "wrong_type")]);
    };
    let mut errors = Vec::new();
    let in_batch = kind.is_none();
    let kind = match kind {
        Some(kind) => kind,
        None => match object.get("type") {
            Some(Value::String(text)) if text == "identify" => Kind::Identify,
            Some(Value::String(text)) if text == "track" => Kind::Track,
            Some(Value::String(_)) => return Err(vec![error(index, "type", "invalid_type")]),
            Some(_) => return Err(vec![error(index, "type", "wrong_type")]),
            None => return Err(vec![error(index, "type", "required")]),
        },
    };

    let allowed: &[&str] = match kind {
        Kind::Identify => &[
            "userId",
            "anonymousId",
            "identifiers",
            "timestamp",
            "messageId",
            "traits",
        ],
        Kind::Track => &[
            "userId",
            "anonymousId",
            "identifiers",
            "timestamp",
            "messageId",
            "event",
            "properties",
        ],
    };
    for name in object.keys() {
        let batch_type = in_batch && name == "type";
        if !allowed.contains(&name.as_str()) && !batch_type {
            errors.push(error(index, name, "unknown_field"));
        }
    }

    let identifiers = identifiers(index, object, &mut errors);
    let timestamp = timestamp(index, object, &mut errors);
    let message_id = message_id(index, object, &mut errors);
    let payload = match kind {
        Kind::Identify => IngestPayload::Identify {
            traits: map_field(index, object, "traits", &mut errors),
        },
        Kind::Track => {
            let event = match object.get("event") {
                None => {
                    errors.push(error(index, "event", "required"));
                    String::new()
                }
                Some(Value::String(name))
                    if validate_event_name(name).is_ok() && !is_reserved_event_name(name) =>
                {
                    name.clone()
                }
                Some(Value::String(name)) if validate_event_name(name).is_ok() => {
                    errors.push(error(index, "event", RESERVED_EVENT_CODE));
                    String::new()
                }
                Some(Value::String(_)) => {
                    errors.push(error(index, "event", "invalid_event_name"));
                    String::new()
                }
                Some(_) => {
                    errors.push(error(index, "event", "wrong_type"));
                    String::new()
                }
            };
            IngestPayload::Track {
                event,
                properties: map_field(index, object, "properties", &mut errors),
            }
        }
    };
    if errors.is_empty() {
        Ok(IngestItem {
            identifiers,
            timestamp,
            message_id,
            payload,
        })
    } else {
        Err(errors)
    }
}

fn map_field(
    index: usize,
    object: &Map<String, Value>,
    name: &str,
    errors: &mut Vec<ItemError>,
) -> Map<String, Value> {
    match object.get(name) {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(map)) => map.clone(),
        Some(_) => {
            errors.push(error(index, name, "wrong_type"));
            Map::new()
        }
    }
}

/// `userId`, `anonymousId` and `identifiers`, as `(kind, value)` pairs. At least one is
/// required; a pair that repeats another, after normalisation, is kept once.
fn identifiers(
    index: usize,
    object: &Map<String, Value>,
    errors: &mut Vec<ItemError>,
) -> Vec<(String, String)> {
    let before = errors.len();
    let mut found: Vec<(String, String)> = Vec::new();
    let mut seen: Vec<Identifier> = Vec::new();
    let mut add = |field: &str, kind: &str, value: &Value, errors: &mut Vec<ItemError>| {
        let Some(text) = value.as_str() else {
            errors.push(error(index, field, "wrong_type"));
            return;
        };
        match Identifier::parse(kind, text) {
            Ok(identifier) => {
                if !seen.contains(&identifier) {
                    seen.push(identifier);
                    found.push((kind.to_owned(), text.to_owned()));
                }
            }
            Err(problem) => errors.push(error(index, field, problem.reason())),
        }
    };
    if let Some(value) = object.get("userId") {
        add("userId", "external:user_id", value, errors);
    }
    if let Some(value) = object.get("anonymousId") {
        add("anonymousId", "anonymous_id", value, errors);
    }
    match object.get("identifiers") {
        None => {}
        Some(Value::Object(map)) if map.len() > MAX_IDENTIFIERS => {
            errors.push(error(index, "identifiers", "too_many_identifiers"));
        }
        Some(Value::Object(map)) => {
            for (kind, value) in map {
                add(&format!("identifiers.{kind}"), kind, value, errors);
            }
        }
        Some(_) => errors.push(error(index, "identifiers", "wrong_type")),
    }
    if found.is_empty() && errors.len() == before {
        errors.push(error(index, "identifiers", "no_identifier"));
    }
    found
}

fn timestamp(
    index: usize,
    object: &Map<String, Value>,
    errors: &mut Vec<ItemError>,
) -> Option<OffsetDateTime> {
    match object.get("timestamp") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => match OffsetDateTime::parse(text, &Rfc3339) {
            Ok(at) => Some(at),
            Err(_) => {
                errors.push(error(index, "timestamp", "invalid_timestamp"));
                None
            }
        },
        Some(_) => {
            errors.push(error(index, "timestamp", "wrong_type"));
            None
        }
    }
}

fn message_id(
    index: usize,
    object: &Map<String, Value>,
    errors: &mut Vec<ItemError>,
) -> Option<String> {
    match object.get("messageId") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => {
            let length = text.chars().count();
            if (1..=MAX_MESSAGE_ID_CHARS).contains(&length) && !text.chars().any(char::is_control) {
                Some(text.clone())
            } else {
                errors.push(error(index, "messageId", "invalid_message_id"));
                None
            }
        }
        Some(_) => {
            errors.push(error(index, "messageId", "wrong_type"));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::macros::datetime;

    use super::*;

    fn one(value: Value, kind: Kind) -> Result<IngestItem, Vec<ItemError>> {
        parse_items(&[value], Some(kind)).map(|mut items| items.remove(0))
    }

    fn errors_of(value: Value, kind: Kind) -> Vec<(String, &'static str)> {
        one(value, kind)
            .unwrap_err()
            .into_iter()
            .map(|e| (e.field, e.reason))
            .collect()
    }

    #[test]
    fn an_identify_maps_user_anonymous_and_named_identifiers() {
        let item = one(
            json!({
                "userId": "u-1",
                "anonymousId": "anon-1",
                "identifiers": {"email": "A@Example.com", "external:crm": "7"},
                "timestamp": "2026-01-01T00:00:00Z",
                "messageId": "m-1",
                "traits": {"plan": "pro"},
            }),
            Kind::Identify,
        )
        .unwrap();

        let mut identifiers = item.identifiers.clone();
        identifiers.sort();
        assert_eq!(
            identifiers,
            [
                ("anonymous_id".to_owned(), "anon-1".to_owned()),
                ("email".to_owned(), "A@Example.com".to_owned()),
                ("external:crm".to_owned(), "7".to_owned()),
                ("external:user_id".to_owned(), "u-1".to_owned()),
            ]
        );
        assert_eq!(item.timestamp, Some(datetime!(2026-01-01 00:00 UTC)));
        assert_eq!(item.message_id.as_deref(), Some("m-1"));
        assert!(
            matches!(item.payload, IngestPayload::Identify { traits } if traits["plan"] == "pro")
        );
    }

    #[test]
    fn a_track_needs_an_event_and_carries_its_properties() {
        let item = one(
            json!({"userId": "u-1", "event": "Order Placed", "properties": {"total": 5}}),
            Kind::Track,
        )
        .unwrap();

        assert!(matches!(
            item.payload,
            IngestPayload::Track { event, properties }
                if event == "Order Placed" && properties["total"] == 5
        ));
        assert_eq!(
            errors_of(json!({"userId": "u-1"}), Kind::Track),
            [("event".to_owned(), "required")]
        );
        assert_eq!(
            errors_of(json!({"userId": "u-1", "event": ""}), Kind::Track),
            [("event".to_owned(), "invalid_event_name")]
        );
        assert_eq!(
            errors_of(json!({"userId": "u-1", "event": 4}), Kind::Track),
            [("event".to_owned(), "wrong_type")]
        );
    }

    #[test]
    fn unknown_top_level_fields_are_rejected_not_dropped() {
        assert_eq!(
            errors_of(
                json!({"userId": "u", "traits": {}, "nickname": "x"}),
                Kind::Identify
            ),
            [("nickname".to_owned(), "unknown_field")]
        );
        assert_eq!(
            errors_of(
                json!({"userId": "u", "event": "e", "traits": {}}),
                Kind::Track
            ),
            [("traits".to_owned(), "unknown_field")]
        );
        assert_eq!(
            errors_of(json!({"userId": "u", "type": "identify"}), Kind::Identify),
            [("type".to_owned(), "unknown_field")]
        );
    }

    #[test]
    fn at_least_one_identifier_is_required() {
        for body in [json!({}), json!({"identifiers": {}}), json!({"traits": {}})] {
            assert_eq!(
                errors_of(body, Kind::Identify),
                [("identifiers".to_owned(), "no_identifier")]
            );
        }
    }

    #[test]
    fn identifiers_are_checked_by_the_core_normalisation() {
        let errors = errors_of(
            json!({
                "userId": 5,
                "anonymousId": "",
                "identifiers": {"email": "nope", "fax": "1", "phone": "abc"},
            }),
            Kind::Identify,
        );

        assert_eq!(
            errors,
            [
                ("userId".to_owned(), "wrong_type"),
                ("anonymousId".to_owned(), "invalid_identifier_value"),
                ("identifiers.email".to_owned(), "invalid_email"),
                ("identifiers.fax".to_owned(), "unknown_identifier_kind"),
                ("identifiers.phone".to_owned(), "invalid_phone"),
            ]
        );
    }

    #[test]
    fn an_identifier_given_twice_is_kept_once() {
        let item = one(
            json!({"userId": "u-1", "identifiers": {"external:user_id": "u-1"}}),
            Kind::Identify,
        )
        .unwrap();

        assert_eq!(item.identifiers.len(), 1);
    }

    #[test]
    fn the_timestamp_must_be_rfc_3339_and_the_message_id_one_to_200_plain_characters() {
        let base = |extra: Value| {
            let mut body = json!({"userId": "u"});
            body.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            body
        };

        assert_eq!(
            errors_of(base(json!({"timestamp": "yesterday"})), Kind::Identify),
            [("timestamp".to_owned(), "invalid_timestamp")]
        );
        assert_eq!(
            errors_of(base(json!({"timestamp": 5})), Kind::Identify),
            [("timestamp".to_owned(), "wrong_type")]
        );
        assert!(one(base(json!({"messageId": "a".repeat(200)})), Kind::Identify).is_ok());
        for bad in [json!(""), json!("a".repeat(201)), json!("a\nb")] {
            assert_eq!(
                errors_of(base(json!({"messageId": bad})), Kind::Identify),
                [("messageId".to_owned(), "invalid_message_id")]
            );
        }
        assert_eq!(
            errors_of(base(json!({"messageId": 7})), Kind::Identify),
            [("messageId".to_owned(), "wrong_type")]
        );
    }

    #[test]
    fn traits_and_properties_must_be_objects() {
        assert_eq!(
            errors_of(json!({"userId": "u", "traits": [1]}), Kind::Identify),
            [("traits".to_owned(), "wrong_type")]
        );
        assert_eq!(
            errors_of(
                json!({"userId": "u", "event": "e", "properties": "x"}),
                Kind::Track
            ),
            [("properties".to_owned(), "wrong_type")]
        );
    }

    #[test]
    fn an_item_that_is_not_an_object_is_a_wrong_type() {
        assert_eq!(
            errors_of(json!([1]), Kind::Identify),
            [("".to_owned(), "wrong_type")]
        );
    }

    #[test]
    fn batch_items_name_their_kind_in_type() {
        let items = parse_items(
            &[
                json!({"type": "identify", "userId": "u", "traits": {"a": 1}}),
                json!({"type": "track", "userId": "u", "event": "e"}),
            ],
            None,
        )
        .unwrap();

        assert_eq!(items[0].payload.kind(), "identify");
        assert_eq!(items[1].payload.kind(), "track");
        for (item, expected) in [
            (json!({"userId": "u"}), "required"),
            (json!({"type": "page", "userId": "u"}), "invalid_type"),
            (json!({"type": 1, "userId": "u"}), "wrong_type"),
        ] {
            let errors = parse_items(&[item], None).unwrap_err();
            assert_eq!(
                (errors[0].index, errors[0].field.as_str(), errors[0].reason),
                (0, "type", expected)
            );
        }
    }

    #[test]
    fn every_invalid_item_is_reported_with_its_index() {
        let errors = parse_items(
            &[
                json!({"type": "track", "userId": "u", "event": "e"}),
                json!({"type": "track", "userId": "u"}),
                json!({"type": "identify", "traits": {}}),
            ],
            None,
        )
        .unwrap_err();

        let found: Vec<(usize, &str, &str)> = errors
            .iter()
            .map(|e| (e.index, e.field.as_str(), e.reason))
            .collect();
        assert_eq!(
            found,
            [
                (1, "event", "required"),
                (2, "identifiers", "no_identifier")
            ]
        );
    }

    #[test]
    fn the_error_list_is_capped() {
        let values: Vec<Value> = (0..500).map(|_| json!({})).collect();

        let errors = parse_items(&values, Some(Kind::Identify)).unwrap_err();

        assert_eq!(errors.len(), MAX_ERRORS);
    }

    #[test]
    fn errors_never_carry_values_and_echo_only_short_plain_field_names() {
        let long = "x".repeat(300);
        let errors = parse_items(
            &[json!({"userId": "secret-value", "traits": "secret-trait", long.clone(): 1})],
            Some(Kind::Identify),
        )
        .unwrap_err();

        let text = format!("{errors:?}");
        assert!(!text.contains("secret"));
        assert!(
            errors
                .iter()
                .all(|e| e.field.chars().count() <= MAX_ECHOED_NAME)
        );
    }

    #[test]
    fn a_batch_body_holds_a_non_empty_array_within_the_limit() {
        let ok = json!({"batch": [{}, {}]});
        assert_eq!(batch_items(&ok, 2).unwrap().len(), 2);

        let cases = [
            (json!({"batch": [{}, {}, {}]}), "batch", "too_many_items"),
            (json!({"batch": []}), "batch", "empty"),
            (json!({"batch": 1}), "batch", "wrong_type"),
            (json!({}), "batch", "required"),
            (json!([]), "batch", "wrong_type"),
            (json!({"batch": [{}], "extra": 1}), "extra", "unknown_field"),
        ];
        for (body, field, reason) in cases {
            let errors = batch_items(&body, 2).unwrap_err();
            assert_eq!(
                (errors[0].field.as_str(), errors[0].reason),
                (field, reason),
                "{body}"
            );
        }
    }
}
