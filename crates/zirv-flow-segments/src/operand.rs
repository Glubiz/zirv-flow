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

/// A bound from the definition as an instant; a date is at UTC midnight.
pub(crate) fn absolute(kind: Kind, value: &Value) -> Option<OffsetDateTime> {
    let text = value.as_str()?;
    if kind == Kind::Date {
        parse_date(text).map(|date| date.midnight().assume_utc())
    } else {
        parse_timestamp(text)
    }
}
