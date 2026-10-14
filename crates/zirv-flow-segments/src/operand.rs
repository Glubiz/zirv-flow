//! Reads a definition's operands (numbers, strings, time bounds), shared by the SQL lowering and
//! the reference evaluator so both take the same operands from the same definition.

use serde_json::Value;
use time::OffsetDateTime;

use crate::time_value::{parse_date, parse_timestamp};
use crate::validate::Kind;

pub(crate) fn number_of(value: &Value) -> Option<f64> {
    value.as_f64().filter(|number| number.is_finite())
}

pub(crate) fn numbers(value: Option<&Value>) -> Option<Vec<f64>> {
    value?.as_array()?.iter().map(number_of).collect()
}

pub(crate) fn strings(value: Option<&Value>) -> Option<Vec<String>> {
    value?
        .as_array()?
        .iter()
        .map(|item| item.as_str().map(str::to_owned))
        .collect()
}

/// An RFC 3339 timestamp from the definition, cut to whole microseconds. PostgreSQL holds
/// microseconds, and the driver rounds a finer bound toward zero from 2000 on, which is up
/// before it, so the SQL and the evaluator both start from the floored instant.
pub(crate) fn timestamp(text: &str) -> Option<OffsetDateTime> {
    let at = parse_timestamp(text)?;
    at.replace_nanosecond(at.nanosecond() / 1_000 * 1_000).ok()
}

/// A bound from the definition as an instant; a date is at UTC midnight.
pub(crate) fn absolute(kind: Kind, value: &Value) -> Option<OffsetDateTime> {
    let text = value.as_str()?;
    if kind == Kind::Date {
        parse_date(text).map(|date| date.midnight().assume_utc())
    } else {
        timestamp(text)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::macros::datetime;

    use super::*;

    #[test]
    fn a_number_must_be_a_finite_number() {
        assert_eq!(number_of(&json!(2.5)), Some(2.5));
        assert_eq!(number_of(&json!("2")), None);
        assert_eq!(number_of(&json!(null)), None);
        assert_eq!(numbers(Some(&json!([1, "2"]))), None);
        assert_eq!(numbers(Some(&json!([1, 2.5]))), Some(vec![1.0, 2.5]));
        assert_eq!(numbers(None), None);
    }

    #[test]
    fn strings_refuse_a_mixed_array() {
        assert_eq!(strings(Some(&json!(["a", 1]))), None);
        assert_eq!(strings(Some(&json!("a"))), None);
        assert_eq!(
            strings(Some(&json!(["a", "b"]))),
            Some(vec!["a".into(), "b".into()])
        );
    }

    #[test]
    fn a_date_is_read_at_utc_midnight() {
        assert_eq!(
            absolute(Kind::Date, &json!("2025-06-15")),
            Some(datetime!(2025-06-15 0:00 UTC))
        );
        assert_eq!(absolute(Kind::Date, &json!("2025-06-15T00:00:00Z")), None);
        assert_eq!(absolute(Kind::Timestamp, &json!(5)), None);
    }

    #[test]
    fn a_bound_is_cut_to_whole_microseconds_before_and_after_2000() {
        for (text, expected) in [
            (
                "1999-12-31T23:59:59.0000005Z",
                datetime!(1999-12-31 23:59:59 UTC),
            ),
            (
                "2025-06-15T12:00:00.1234567Z",
                datetime!(2025-06-15 12:00:00.123456 UTC),
            ),
            (
                "1969-12-31T23:59:59.9999999+00:00",
                datetime!(1969-12-31 23:59:59.999999 UTC),
            ),
        ] {
            assert_eq!(
                absolute(Kind::Timestamp, &json!(text)),
                Some(expected),
                "{text}"
            );
        }
    }
}
