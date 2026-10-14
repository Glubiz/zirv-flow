//! A SQL text and the values bound to it.

use serde_json::{Number, Value, json};
use time::OffsetDateTime;
use time::UtcOffset;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

/// One bound value. The SQL casts each placeholder to the matching type.
#[derive(Clone, Debug, PartialEq)]
pub enum Param {
    Text(String),
    TextArray(Vec<String>),
    Float(f64),
    FloatArray(Vec<f64>),
    Int(i64),
    Timestamp(OffsetDateTime),
    Uuid(Uuid),
    UuidArray(Vec<Uuid>),
    Bool(bool),
    Jsonb(Value),
}

impl Param {
    /// A tagged JSON form, for fixtures and logs. A timestamp is RFC 3339 in UTC.
    pub fn to_value(&self) -> Value {
        match self {
            Self::Text(text) => json!({"text": text}),
            Self::TextArray(items) => json!({"text_array": items}),
            Self::Float(number) => json!({"float": float(*number)}),
            Self::FloatArray(numbers) => {
                json!({"float_array": numbers.iter().map(|number| float(*number)).collect::<Vec<_>>()})
            }
            Self::Int(number) => json!({"int": number}),
            Self::Timestamp(at) => json!({"timestamp": timestamp(*at)}),
            Self::Uuid(id) => json!({"uuid": id.to_string()}),
            Self::UuidArray(ids) => {
                json!({"uuid_array": ids.iter().map(Uuid::to_string).collect::<Vec<_>>()})
            }
            Self::Bool(flag) => json!({"bool": flag}),
            Self::Jsonb(value) => json!({"jsonb": value}),
        }
    }
}

fn float(number: f64) -> Value {
    Number::from_f64(number).map_or(Value::Null, Value::Number)
}

fn timestamp(at: OffsetDateTime) -> String {
    at.to_offset(UtcOffset::UTC)
        .format(&Rfc3339)
        .unwrap_or_default()
}

/// A query. The caller binds `$1` (the workspace id) first, then `params` in order as `$2`
/// onward. [`crate::CompiledSegment::batch_sql`] also takes a batch of person ids, bound last
/// as a `uuid[]`.
#[derive(Clone, Debug, PartialEq)]
pub struct Query {
    pub sql: String,
    pub params: Vec<Param>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn every_param_has_a_tagged_json_form() {
        let id = Uuid::nil();
        let cases = [
            (Param::Text("a".into()), json!({"text": "a"})),
            (
                Param::TextArray(vec!["a".into()]),
                json!({"text_array": ["a"]}),
            ),
            (Param::Float(1.5), json!({"float": 1.5})),
            (
                Param::FloatArray(vec![1.0, 2.5]),
                json!({"float_array": [1.0, 2.5]}),
            ),
            (Param::Int(3), json!({"int": 3})),
            (
                Param::Timestamp(datetime!(2025-01-02 03:04:05 +02:00)),
                json!({"timestamp": "2025-01-02T01:04:05Z"}),
            ),
            (Param::Uuid(id), json!({"uuid": id.to_string()})),
            (
                Param::UuidArray(vec![id]),
                json!({"uuid_array": [id.to_string()]}),
            ),
            (Param::Bool(true), json!({"bool": true})),
            (Param::Jsonb(json!({"k": 1})), json!({"jsonb": {"k": 1}})),
        ];
        for (param, expected) in cases {
            assert_eq!(param.to_value(), expected);
        }
    }
}
