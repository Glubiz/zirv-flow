//! One Kafka message as one record, or as a refusal that holds no payload byte (spec D2, D5).

use chrono::SecondsFormat;
use rskafka::record::RecordAndOffset;
use serde_json::Value;
use zirv_flow_core::csv_file::MAX_RECORD_BYTES;

use super::super::types::{Record, Refused};

/// The most characters a record key may have: the message key.
pub(crate) const MAX_KEY_CHARS: usize = 512;

pub(crate) const KEY_MISSING: &str = "kafka_key_missing";
pub(crate) const KEY_NOT_TEXT: &str = "kafka_key_not_text";
pub(crate) const VALUE_NOT_JSON: &str = "kafka_value_not_json";
pub(crate) const VALUE_NOT_OBJECT: &str = "kafka_value_not_object";
pub(crate) const MESSAGE_TOO_LARGE: &str = "kafka_message_too_large";

/// The virtual field holding the message key.
pub(crate) const KEY_FIELD: &str = "_key";
/// The virtual field holding the message time.
pub(crate) const TIMESTAMP_FIELD: &str = "_timestamp";

/// What one message became.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum Converted {
    Record(Record),
    Refused(Refused),
}

fn refuse(partition: i32, offset: i64, code: &'static str) -> Converted {
    Converted::Refused(Refused {
        location: format!("partition {partition} offset {offset}"),
        code,
    })
}

/// Converts the message at `partition`/`offset`. The key is the message key as text; a null
/// value is a tombstone; any other value is a JSON object with `_key` and `_timestamp` added
/// (they win over value fields of the same name). Errors name the place, never the content.
pub(super) fn convert(partition: i32, message: &RecordAndOffset) -> Converted {
    let offset = message.offset;
    let record = &message.record;
    let Some(key) = &record.key else {
        return refuse(partition, offset, KEY_MISSING);
    };
    let Ok(key) = std::str::from_utf8(key) else {
        return refuse(partition, offset, KEY_NOT_TEXT);
    };
    let length = key.chars().count();
    if length == 0 {
        return refuse(partition, offset, KEY_MISSING);
    }
    if length > MAX_KEY_CHARS {
        return refuse(partition, offset, KEY_NOT_TEXT);
    }
    let occurrence = Some(format!("{partition}/{offset}"));
    let Some(value) = &record.value else {
        return Converted::Record(Record {
            key: key.to_owned(),
            data: Value::Null,
            deleted: true,
            occurrence,
        });
    };
    if value.len() > MAX_RECORD_BYTES {
        return refuse(partition, offset, MESSAGE_TOO_LARGE);
    }
    let Ok(parsed) = serde_json::from_slice::<Value>(value) else {
        return refuse(partition, offset, VALUE_NOT_JSON);
    };
    let Value::Object(mut object) = parsed else {
        return refuse(partition, offset, VALUE_NOT_OBJECT);
    };
    object.insert(KEY_FIELD.to_owned(), Value::String(key.to_owned()));
    object.insert(
        TIMESTAMP_FIELD.to_owned(),
        Value::String(
            record
                .timestamp
                .to_rfc3339_opts(SecondsFormat::Millis, true),
        ),
    );
    Converted::Record(Record {
        key: key.to_owned(),
        data: Value::Object(object),
        deleted: false,
        occurrence,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::{TimeZone, Utc};
    use rskafka::record::Record as KafkaRecord;
    use serde_json::json;

    use super::*;

    fn message(key: Option<&[u8]>, value: Option<&[u8]>) -> RecordAndOffset {
        RecordAndOffset {
            record: KafkaRecord {
                key: key.map(<[u8]>::to_vec),
                value: value.map(<[u8]>::to_vec),
                headers: BTreeMap::new(),
                timestamp: Utc.timestamp_millis_opt(1_700_000_000_123).unwrap(),
            },
            offset: 42,
        }
    }

    fn refused(converted: Converted) -> (String, &'static str) {
        match converted {
            Converted::Refused(refused) => (refused.location, refused.code),
            Converted::Record(_) => panic!("expected a refusal"),
        }
    }

    #[test]
    fn a_json_object_is_a_record_with_the_key_the_time_and_the_occurrence() {
        let converted = convert(3, &message(Some(b"u-1"), Some(br#"{"email":"a@b.c"}"#)));

        let Converted::Record(record) = converted else {
            panic!("expected a record");
        };
        assert_eq!(record.key, "u-1");
        assert!(!record.deleted);
        assert_eq!(record.occurrence.as_deref(), Some("3/42"));
        assert_eq!(
            record.data,
            json!({"email": "a@b.c", "_key": "u-1", "_timestamp": "2023-11-14T22:13:20.123Z"})
        );
    }

    #[test]
    fn the_virtual_fields_win_over_value_fields_of_the_same_name() {
        let converted = convert(
            0,
            &message(Some(b"k"), Some(br#"{"_key":"other","_timestamp":1}"#)),
        );

        let Converted::Record(record) = converted else {
            panic!("expected a record");
        };
        assert_eq!(record.data["_key"], "k");
        assert_eq!(record.data["_timestamp"], "2023-11-14T22:13:20.123Z");
    }

    #[test]
    fn a_null_value_is_a_tombstone_that_carries_only_the_key() {
        let converted = convert(1, &message(Some(b"gone"), None));

        assert_eq!(
            converted,
            Converted::Record(Record {
                key: "gone".into(),
                data: Value::Null,
                deleted: true,
                occurrence: Some("1/42".into()),
            })
        );
    }

    #[test]
    fn each_malformed_message_is_refused_with_its_place_and_a_static_code() {
        let big = vec![b' '; MAX_RECORD_BYTES + 1];
        let long_key = "k".repeat(MAX_KEY_CHARS + 1);
        let cases: Vec<(RecordAndOffset, &str)> = vec![
            (message(None, Some(b"{}")), KEY_MISSING),
            (message(Some(b""), Some(b"{}")), KEY_MISSING),
            (message(None, None), KEY_MISSING),
            (message(Some(&[0xff, 0xfe]), Some(b"{}")), KEY_NOT_TEXT),
            (
                message(Some(long_key.as_bytes()), Some(b"{}")),
                KEY_NOT_TEXT,
            ),
            (message(Some(b"k"), Some(b"")), VALUE_NOT_JSON),
            (message(Some(b"k"), Some(b"{oops")), VALUE_NOT_JSON),
            (message(Some(b"k"), Some(b"[1]")), VALUE_NOT_OBJECT),
            (message(Some(b"k"), Some(b"\"text\"")), VALUE_NOT_OBJECT),
            (message(Some(b"k"), Some(&big)), MESSAGE_TOO_LARGE),
        ];

        for (input, code) in cases {
            let (location, got) = refused(convert(5, &input));
            assert_eq!(got, code);
            assert_eq!(location, "partition 5 offset 42");
        }
    }

    #[test]
    fn a_refusal_holds_neither_the_key_nor_the_value() {
        let converted = convert(
            0,
            &message(Some(b"secret-key"), Some(b"secret-value not json")),
        );

        let rendered = format!("{converted:?}");

        assert!(!rendered.contains("secret"), "{rendered}");
    }

    #[test]
    fn a_key_of_exactly_the_limit_is_accepted() {
        let key = "é".repeat(MAX_KEY_CHARS);

        assert!(matches!(
            convert(0, &message(Some(key.as_bytes()), None)),
            Converted::Record(_)
        ));
    }
}
