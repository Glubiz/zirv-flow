//! The send body of a provider destination: the message fields placed at the JSON pointers the
//! manifest names, beside its static values.

use serde_json::{Map, Value};

use super::super::error::DestinationError;
use crate::manifest::{MessageField, SendSpec, place};

fn malformed(field: &str) -> DestinationError {
    DestinationError::Config {
        field: format!("message.{field}"),
        reason: "is not usable".to_owned(),
    }
}

/// A non-empty text of the payload at `key`.
fn text<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

/// The JSON body of one email: `payload` is the email payload of the send pipeline (sender, reply
/// address, subject, parts, extra headers), `to` the recipient's address and `message_id` the
/// message's id. A field the manifest does not place is left out; an optional part the message
/// does not have (a display name, a reply address, a text part) is not placed.
pub(super) fn build(
    send: &SendSpec,
    payload: &Value,
    to: &str,
    message_id: &str,
) -> Result<Value, DestinationError> {
    let mut body = Value::Null;
    let failed = |_| DestinationError::Config {
        field: "manifest".to_owned(),
        reason: "does not make a valid body".to_owned(),
    };
    for (pointer, value) in &send.statics {
        place(&mut body, pointer, value.clone()).map_err(failed)?;
    }
    for (field, pointer) in &send.fields {
        let value = match field {
            MessageField::From => Some(
                payload
                    .pointer("/from/address")
                    .and_then(Value::as_str)
                    .filter(|address| !address.is_empty())
                    .map(Value::from)
                    .ok_or_else(|| malformed("from"))?,
            ),
            MessageField::FromName => payload
                .pointer("/from/name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .map(Value::from),
            MessageField::ReplyTo => text(payload, "reply_to").map(Value::from),
            MessageField::To => Some(Value::from(to)),
            MessageField::Subject => Some(
                payload
                    .get("subject")
                    .and_then(Value::as_str)
                    .map(Value::from)
                    .ok_or_else(|| malformed("subject"))?,
            ),
            MessageField::Html => text(payload, "html").map(Value::from),
            MessageField::Text => text(payload, "text").map(Value::from),
            MessageField::Headers => {
                let headers: Map<String, Value> = payload
                    .get("headers")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                (!headers.is_empty()).then_some(Value::Object(headers))
            }
            MessageField::MessageId => Some(Value::from(message_id)),
        };
        if let Some(value) = value {
            place(&mut body, pointer, value).map_err(failed)?;
        }
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::manifest::validate;

    fn send() -> SendSpec {
        let manifest = json!({
            "version": "v1",
            "base_url": "https://api.mailer.example.test",
            "auth": {"type": "bearer", "credential": {"fields": [
                {"key": "token", "label": "T", "type": "string", "required": true, "secret": true},
                {"key": "hook", "label": "H", "type": "string", "required": true, "secret": true}]}},
            "destination": {
                "send": {"path": "/m", "idempotency_header": "Idempotency-Key", "body": {
                    "fields": {"from": "/sender/address", "from_name": "/sender/name",
                               "reply_to": "/reply_to", "to": "/rcpt/0", "subject": "/subject",
                               "html": "/content/html", "text": "/content/text",
                               "headers": "/headers", "message_id": "/meta/id"},
                    "static": {"/stream": "outbound", "/opens": true, "/priority": 3}}},
                "feedback": {"event_type": "/e", "events": {"delivered": ["d"]},
                    "message_id": "/m", "verification": {"signature_header": "X-S",
                    "secret_field": "hook"}}
            }
        });
        validate(&manifest).unwrap().provider.unwrap().send
    }

    fn payload() -> Value {
        json!({
            "from": {"name": "News", "address": "news@example.test"},
            "reply_to": "reply@example.test",
            "subject": "Hello",
            "html": "<p>Hi</p>",
            "text": "Hi",
            "headers": {"List-Unsubscribe": "<https://x.test/u>"}
        })
    }

    #[test]
    fn every_field_lands_where_the_manifest_puts_it_beside_the_static_values() {
        let body = build(&send(), &payload(), "ada@example.test", "msg-1").unwrap();

        assert_eq!(
            body,
            json!({
                "stream": "outbound", "opens": true, "priority": 3,
                "sender": {"address": "news@example.test", "name": "News"},
                "reply_to": "reply@example.test",
                "rcpt": ["ada@example.test"],
                "subject": "Hello",
                "content": {"html": "<p>Hi</p>", "text": "Hi"},
                "headers": {"List-Unsubscribe": "<https://x.test/u>"},
                "meta": {"id": "msg-1"}
            })
        );
    }

    #[test]
    fn an_optional_part_the_message_lacks_is_not_placed() {
        let payload = json!({
            "from": {"name": "", "address": "news@example.test"},
            "reply_to": null, "subject": "Hi", "html": "<p>x</p>", "text": "", "headers": {}
        });

        let body = build(&send(), &payload, "ada@example.test", "msg-1").unwrap();

        assert!(body["sender"].get("name").is_none());
        assert!(body.get("reply_to").is_none());
        assert!(body["content"].get("text").is_none());
        assert!(body.get("headers").is_none());
    }

    #[test]
    fn a_message_without_a_sender_or_subject_is_a_setting_error_that_names_the_field_only() {
        let no_sender = json!({"subject": "Hi", "html": "x"});
        let no_subject = json!({"from": {"address": "a@example.test"}, "html": "x"});

        for (payload, field) in [(no_sender, "message.from"), (no_subject, "message.subject")] {
            let error = build(&send(), &payload, "ada@example.test", "m").unwrap_err();
            assert_eq!(
                error,
                DestinationError::Config {
                    field: field.to_owned(),
                    reason: "is not usable".to_owned()
                }
            );
        }
    }
}
