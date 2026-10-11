//! The transforms, one function each over a JSON value (a cell starts as a string).

use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone, Utc};
use serde_json::{Number, Value};

use super::apply::{Cells, RowErrorReason};
use super::model::Transform;
use crate::people::{TraitType, TraitValue, coerce};

/// Applies `transform` to `value`. `own` is the column the rule is on. A failure names the
/// column it concerns and the rule it broke.
pub(super) fn apply_transform(
    transform: &Transform,
    value: Value,
    row: &dyn Cells,
    own: &str,
) -> Result<Value, (String, RowErrorReason)> {
    let fail = |reason| (own.to_owned(), reason);
    match transform {
        Transform::Trim => Ok(map_text(value, |text| text.trim().to_owned())),
        Transform::Lower => Ok(map_text(value, |text| text.to_lowercase())),
        Transform::Upper => Ok(map_text(value, |text| text.to_uppercase())),
        Transform::Cast { to } => cast(*to, value).map_err(fail),
        Transform::Timestamp { format } => timestamp(format, value).map_err(fail),
        Transform::Concat { columns, separator } => {
            let mut parts = vec![display(&value)];
            for other in columns {
                let cell = row
                    .cell(other)
                    .ok_or_else(|| (other.clone(), RowErrorReason::UnknownColumn))?;
                parts.push(cell.to_owned());
            }
            parts.retain(|part| !part.trim().is_empty());
            Ok(Value::String(parts.join(separator)))
        }
        Transform::Constant { value } => Ok(Value::String(value.clone())),
        Transform::ValueMap { map } => Ok(match value.as_str().and_then(|key| map.get(key)) {
            Some(replacement) => Value::String(replacement.clone()),
            None => value,
        }),
        Transform::Split { separator } => Ok(match value.as_str() {
            Some("") | None => value,
            Some(whole) => Value::Array(
                whole
                    .split(separator.as_str())
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .map(|part| Value::String(part.to_owned()))
                    .collect(),
            ),
        }),
    }
}

fn map_text(value: Value, change: impl FnOnce(&str) -> String) -> Value {
    match value {
        Value::String(text) => Value::String(change(&text)),
        other => other,
    }
}

/// A value as text for joining.
fn display(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Converts `value` to `kind`. An empty text stays empty: it is an absent value, not an error.
pub(super) fn cast(kind: TraitType, value: Value) -> Result<Value, RowErrorReason> {
    let typed = match &value {
        Value::String(raw) => {
            let raw = raw.trim();
            if raw.is_empty() {
                return Ok(Value::String(String::new()));
            }
            match kind {
                TraitType::String | TraitType::Timestamp | TraitType::Date => {
                    Value::String(raw.to_owned())
                }
                TraitType::Number => parse_number(raw).ok_or(RowErrorReason::WrongType)?,
                TraitType::Boolean => {
                    Value::Bool(parse_flag(raw).ok_or(RowErrorReason::WrongType)?)
                }
                TraitType::StringList => return Err(RowErrorReason::WrongType),
            }
        }
        other => other.clone(),
    };
    let failure = if kind == TraitType::Timestamp {
        RowErrorReason::BadTimestamp
    } else {
        RowErrorReason::WrongType
    };
    coerce(kind, &typed).map(into_json).map_err(|_| failure)
}

fn parse_number(raw: &str) -> Option<Value> {
    if let Ok(integer) = raw.parse::<i64>() {
        return Some(Value::Number(integer.into()));
    }
    if let Ok(integer) = raw.parse::<u64>() {
        return Some(Value::Number(integer.into()));
    }
    raw.parse::<f64>()
        .ok()
        .filter(|number| number.is_finite())
        .and_then(Number::from_f64)
        .map(Value::Number)
}

/// A cell read as a yes or a no; `None` for anything else. An empty cell is a no.
pub(super) fn parse_flag(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "t" | "yes" | "y" | "1" | "x" | "on" => Some(true),
        "false" | "f" | "no" | "n" | "0" | "off" | "" => Some(false),
        _ => None,
    }
}

pub(super) fn into_json(value: TraitValue) -> Value {
    match value {
        TraitValue::String(text) | TraitValue::Timestamp(text) | TraitValue::Date(text) => {
            Value::String(text)
        }
        TraitValue::Number(number) => Value::Number(number),
        TraitValue::Boolean(flag) => Value::Bool(flag),
        TraitValue::StringList(items) => {
            Value::Array(items.into_iter().map(Value::String).collect())
        }
    }
}

/// Whether `format` is a pattern `timestamp` can use.
pub(super) fn valid_format(format: &str) -> bool {
    use chrono::format::{Item, StrftimeItems};

    if matches!(format, "unix" | "unix_ms") {
        return true;
    }
    !format.is_empty()
        && format.len() <= 64
        && StrftimeItems::new(format).all(|item| !matches!(item, Item::Error))
}

fn timestamp(format: &str, value: Value) -> Result<Value, RowErrorReason> {
    let Some(raw) = value.as_str() else {
        return Err(RowErrorReason::BadTimestamp);
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Value::String(String::new()));
    }
    let parsed: Option<DateTime<Utc>> = match format {
        "unix" => raw
            .parse::<i64>()
            .ok()
            .and_then(|seconds| Utc.timestamp_opt(seconds, 0).single()),
        "unix_ms" => raw
            .parse::<i64>()
            .ok()
            .and_then(|millis| Utc.timestamp_millis_opt(millis).single()),
        _ => DateTime::parse_from_str(raw, format)
            .map(|parsed| parsed.to_utc())
            .ok()
            .or_else(|| {
                NaiveDateTime::parse_from_str(raw, format)
                    .ok()
                    .map(|naive| naive.and_utc())
            })
            .or_else(|| {
                NaiveDate::parse_from_str(raw, format)
                    .ok()
                    .and_then(|date| date.and_hms_opt(0, 0, 0))
                    .map(|naive| naive.and_utc())
            }),
    };
    let parsed = parsed.ok_or(RowErrorReason::BadTimestamp)?;
    Ok(Value::String(
        parsed.to_rfc3339_opts(SecondsFormat::AutoSi, true),
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;

    struct Row(Vec<(&'static str, &'static str)>);

    impl Cells for Row {
        fn cell(&self, column: &str) -> Option<&str> {
            self.0
                .iter()
                .find(|(name, _)| *name == column)
                .map(|(_, cell)| *cell)
        }
    }

    fn run(transform: Transform, value: &str) -> Result<Value, RowErrorReason> {
        let row = Row(vec![("a", "x"), ("b", "y"), ("blank", "  ")]);
        apply_transform(&transform, json!(value), &row, "own").map_err(|(_, reason)| reason)
    }

    #[test]
    fn trim_lower_and_upper_change_text() {
        assert_eq!(run(Transform::Trim, "  a b "), Ok(json!("a b")));
        assert_eq!(run(Transform::Lower, "AbC"), Ok(json!("abc")));
        assert_eq!(run(Transform::Upper, "AbC"), Ok(json!("ABC")));
    }

    #[test]
    fn cast_converts_text_to_each_registry_type() {
        let cast = |to, text| run(Transform::Cast { to }, text);

        assert_eq!(cast(TraitType::Number, " 42 "), Ok(json!(42)));
        assert_eq!(cast(TraitType::Number, "1.5"), Ok(json!(1.5)));
        assert_eq!(
            cast(TraitType::Number, "abc"),
            Err(RowErrorReason::WrongType)
        );
        assert_eq!(
            cast(TraitType::Number, "NaN"),
            Err(RowErrorReason::WrongType)
        );
        assert_eq!(cast(TraitType::Boolean, "Yes"), Ok(json!(true)));
        assert_eq!(cast(TraitType::Boolean, "0"), Ok(json!(false)));
        assert_eq!(
            cast(TraitType::Boolean, "maybe"),
            Err(RowErrorReason::WrongType)
        );
        assert_eq!(cast(TraitType::String, " s "), Ok(json!("s")));
        assert_eq!(cast(TraitType::Date, "2026-02-28"), Ok(json!("2026-02-28")));
        assert_eq!(
            cast(TraitType::Date, "28/02/2026"),
            Err(RowErrorReason::WrongType)
        );
        assert_eq!(
            cast(TraitType::Timestamp, "2026-05-01T12:00:00+02:00"),
            Ok(json!("2026-05-01T10:00:00Z"))
        );
        assert_eq!(
            cast(TraitType::Timestamp, "yesterday"),
            Err(RowErrorReason::BadTimestamp)
        );
        assert_eq!(
            cast(TraitType::StringList, "a,b"),
            Err(RowErrorReason::WrongType)
        );
    }

    #[test]
    fn an_empty_cell_stays_empty_through_a_cast() {
        assert_eq!(
            run(
                Transform::Cast {
                    to: TraitType::Number
                },
                "  "
            ),
            Ok(json!(""))
        );
    }

    #[test]
    fn timestamp_parses_the_given_format_as_utc() {
        let parse = |format: &str, text| {
            run(
                Transform::Timestamp {
                    format: format.into(),
                },
                text,
            )
        };

        assert_eq!(
            parse("%d/%m/%Y %H:%M", "31/12/2026 23:30"),
            Ok(json!("2026-12-31T23:30:00Z"))
        );
        assert_eq!(
            parse("%d.%m.%Y", "01.02.2026"),
            Ok(json!("2026-02-01T00:00:00Z"))
        );
        assert_eq!(
            parse("%Y-%m-%d %H:%M %z", "2026-05-01 12:00 +0200"),
            Ok(json!("2026-05-01T10:00:00Z"))
        );
        assert_eq!(
            parse("unix", "1767225600"),
            Ok(json!("2026-01-01T00:00:00Z"))
        );
        assert_eq!(
            parse("unix_ms", "1767225600500"),
            Ok(json!("2026-01-01T00:00:00.500Z"))
        );
        assert_eq!(
            parse("%d/%m/%Y", "2026-01-01"),
            Err(RowErrorReason::BadTimestamp)
        );
        assert_eq!(
            parse("%d/%m/%Y", "32/01/2026"),
            Err(RowErrorReason::BadTimestamp)
        );
        assert_eq!(parse("unix", "soon"), Err(RowErrorReason::BadTimestamp));
        assert_eq!(parse("%d/%m/%Y", " "), Ok(json!("")));
    }

    #[test]
    fn only_real_strftime_patterns_are_valid_formats() {
        assert!(valid_format("%Y-%m-%d"));
        assert!(valid_format("unix"));
        assert!(!valid_format(""));
        assert!(!valid_format("%Q"));
        assert!(!valid_format(&"%Y".repeat(40)));
    }

    #[test]
    fn concat_joins_the_value_with_other_columns_and_skips_empty_ones() {
        let join = |columns: &[&str]| {
            run(
                Transform::Concat {
                    columns: columns.iter().map(|c| (*c).to_owned()).collect(),
                    separator: " ".into(),
                },
                "first",
            )
        };

        assert_eq!(join(&["a", "b"]), Ok(json!("first x y")));
        assert_eq!(join(&["blank", "a"]), Ok(json!("first x")));
        assert_eq!(join(&["nope"]), Err(RowErrorReason::UnknownColumn));
    }

    #[test]
    fn concat_reports_the_missing_column_by_name() {
        let row = Row(vec![]);
        let error = apply_transform(
            &Transform::Concat {
                columns: vec!["ghost".into()],
                separator: "-".into(),
            },
            json!("v"),
            &row,
            "own",
        )
        .unwrap_err();

        assert_eq!(error, ("ghost".to_owned(), RowErrorReason::UnknownColumn));
    }

    #[test]
    fn constant_ignores_the_cell() {
        assert_eq!(
            run(
                Transform::Constant {
                    value: "fixed".into()
                },
                "anything"
            ),
            Ok(json!("fixed"))
        );
    }

    #[test]
    fn value_map_replaces_listed_values_and_keeps_the_rest() {
        let map: BTreeMap<String, String> = [("M", "male"), ("F", "female")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();

        assert_eq!(
            run(Transform::ValueMap { map: map.clone() }, "M"),
            Ok(json!("male"))
        );
        assert_eq!(run(Transform::ValueMap { map }, "X"), Ok(json!("X")));
    }

    #[test]
    fn split_makes_a_trimmed_list_without_empty_parts() {
        assert_eq!(
            run(
                Transform::Split {
                    separator: ";".into()
                },
                " a ; b;;c "
            ),
            Ok(json!(["a", "b", "c"]))
        );
        assert_eq!(
            run(
                Transform::Split {
                    separator: ";".into()
                },
                ""
            ),
            Ok(json!(""))
        );
    }

    #[test]
    fn a_flag_reads_the_usual_yes_and_no_words() {
        for yes in ["true", "TRUE", "1", "yes", "x", "X", "y", "on"] {
            assert_eq!(parse_flag(yes), Some(true), "{yes}");
        }
        for no in ["false", "0", "no", "n", "", "  ", "off"] {
            assert_eq!(parse_flag(no), Some(false), "{no}");
        }
        assert_eq!(parse_flag("maybe"), None);
    }
}
