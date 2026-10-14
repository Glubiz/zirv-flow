//! One value compared with a condition, the way the compiled SQL does it (architecture 8.1,
//! ruling 887b9eef): two-valued, so a missing, null or wrongly typed value is false for every
//! operator but `not_exists`, and `not_in`, `neq` and the rest do not count it as different.

use serde_json::Value;
use zirv_flow_core::segment::Op;

use crate::stored_time;
use crate::time_value::{parse_date, parse_duration, parse_timestamp};
use crate::validate::Kind;

/// A number past this magnitude is treated as missing (ruling e6659f67).
const NUMBER_BOUND: f64 = 1e300;
const HOUR: i64 = 3_600 * 1_000_000;

/// Whether a stored value is present: the key is there and is not JSON null.
pub(crate) fn present(stored: Option<&Value>) -> bool {
    stored.is_some_and(|value| !value.is_null())
}

/// Evaluates one leaf over the stored value, if any. `now` is microseconds since the epoch.
pub(crate) fn leaf(
    kind: Kind,
    stored: Option<&Value>,
    op: Op,
    wanted: Option<&Value>,
    now: i64,
) -> bool {
    match op {
        Op::Exists => return present(stored),
        Op::NotExists => return !present(stored),
        _ => {}
    }
    match kind {
        Kind::String => string(stored, op, wanted),
        Kind::Number | Kind::Integer => {
            number(stored_number(stored, kind == Kind::Integer), op, wanted)
        }
        Kind::Boolean => boolean(stored, op, wanted),
        Kind::Timestamp | Kind::Date => time(stored_time_of(kind, stored), kind, op, wanted, now),
        Kind::StringList => list(stored, op, wanted),
        Kind::Object => false,
    }
}

/// The stored value as `float8`: a number of at most `1e300` in magnitude, and whole when the
/// property is an integer.
pub(crate) fn stored_number(stored: Option<&Value>, integral: bool) -> Option<f64> {
    let Some(Value::Number(number)) = stored else {
        return None;
    };
    let value = number.as_f64()?;
    if value.abs() > NUMBER_BOUND || (integral && value != value.trunc()) {
        return None;
    }
    Some(value)
}

/// The stored value as an instant: a string of the write path's shape that PostgreSQL accepts.
pub(crate) fn stored_time_of(kind: Kind, stored: Option<&Value>) -> Option<i64> {
    let text = stored?.as_str()?;
    if kind == Kind::Date {
        stored_time::date(text)
    } else {
        stored_time::timestamp(text)
    }
}

fn boolean(stored: Option<&Value>, op: Op, wanted: Option<&Value>) -> bool {
    matches!(
        (op, stored, wanted),
        (Op::Eq, Some(Value::Bool(have)), Some(Value::Bool(want))) if have == want
    )
}

fn string(stored: Option<&Value>, op: Op, wanted: Option<&Value>) -> bool {
    let Some(Value::String(have)) = stored else {
        return false;
    };
    let text = wanted.and_then(Value::as_str);
    match op {
        Op::Eq => text == Some(have.as_str()),
        Op::Neq => text.is_some_and(|want| want != have),
        Op::Contains => text.is_some_and(|want| have.contains(want)),
        Op::StartsWith => text.is_some_and(|want| have.starts_with(want)),
        Op::In => strings(wanted).is_some_and(|items| items.contains(have)),
        Op::NotIn => strings(wanted).is_some_and(|items| !items.contains(have)),
        _ => false,
    }
}

/// Compares a number: a trait, a property or an aggregate. `None` is a missing value.
pub(crate) fn number(have: Option<f64>, op: Op, wanted: Option<&Value>) -> bool {
    let Some(have) = have else {
        return false;
    };
    match op {
        Op::Between => numbers(wanted).is_some_and(|bounds| match bounds.as_slice() {
            [start, end] => *start <= have && have <= *end,
            _ => false,
        }),
        Op::In => numbers(wanted).is_some_and(|items| items.contains(&have)),
        Op::NotIn => numbers(wanted).is_some_and(|items| !items.contains(&have)),
        _ => {
            let Some(want) = wanted.and_then(number_of) else {
                return false;
            };
            match op {
                Op::Eq => have == want,
                Op::Neq => have != want,
                Op::Gt => have > want,
                Op::Gte => have >= want,
                Op::Lt => have < want,
                Op::Lte => have <= want,
                _ => false,
            }
        }
    }
}

/// Compares an instant. `None` is a missing value. A date was read at UTC midnight already.
pub(crate) fn time(at: Option<i64>, kind: Kind, op: Op, wanted: Option<&Value>, now: i64) -> bool {
    let Some(at) = at else {
        return false;
    };
    let bound = |value: &Value| absolute(kind, value);
    match op {
        Op::Before => wanted.and_then(bound).is_some_and(|bound| at < bound),
        Op::After => wanted.and_then(bound).is_some_and(|bound| at > bound),
        Op::Between => match wanted.and_then(Value::as_array).map(Vec::as_slice) {
            Some([start, end]) => match (bound(start), bound(end)) {
                (Some(start), Some(end)) => start <= at && at < end,
                _ => false,
            },
            _ => false,
        },
        Op::WithinLast | Op::MoreThanAgo | Op::WithinNext => {
            let Some(span) = wanted.and_then(Value::as_str).and_then(span_of) else {
                return false;
            };
            match op {
                Op::WithinLast => now - span < at && at <= now,
                Op::MoreThanAgo => at <= now - span,
                _ => now <= at && at < now + span,
            }
        }
        _ => false,
    }
}

fn list(stored: Option<&Value>, op: Op, wanted: Option<&Value>) -> bool {
    let Some(Value::Array(items)) = stored else {
        return false;
    };
    let held = |item: &String| {
        items
            .iter()
            .any(|have| have.as_str() == Some(item.as_str()))
    };
    match op {
        Op::IsEmpty => items.is_empty(),
        Op::Contains => strings(wanted).is_some_and(|wanted| wanted.iter().any(held)),
        Op::ContainsAll => strings(wanted).is_some_and(|wanted| wanted.iter().all(held)),
        _ => false,
    }
}

fn number_of(value: &Value) -> Option<f64> {
    value.as_f64().filter(|number| number.is_finite())
}

fn numbers(value: Option<&Value>) -> Option<Vec<f64>> {
    value?.as_array()?.iter().map(number_of).collect()
}

fn strings(value: Option<&Value>) -> Option<Vec<String>> {
    value?
        .as_array()?
        .iter()
        .map(|item| item.as_str().map(str::to_owned))
        .collect()
}

/// A bound from the definition as an instant; a date is at UTC midnight.
pub(crate) fn absolute(kind: Kind, value: &Value) -> Option<i64> {
    let text = value.as_str()?;
    let at = if kind == Kind::Date {
        parse_date(text)?.midnight().assume_utc()
    } else {
        parse_timestamp(text)?
    };
    Some(stored_time::micros(at))
}

/// A window or relative span in microseconds.
pub(crate) fn span_of(text: &str) -> Option<i64> {
    let hours = parse_duration(text).ok()?;
    i64::try_from(hours).ok()?.checked_mul(HOUR)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const NOW: i64 = 1_750_000_000_000_000;

    fn check(kind: Kind, stored: Value, op: Op, wanted: Option<Value>) -> bool {
        leaf(kind, Some(&stored), op, wanted.as_ref(), NOW)
    }

    #[test]
    fn a_missing_or_null_value_fails_every_operator_but_not_exists() {
        for op in [Op::Eq, Op::Neq, Op::NotIn, Op::Exists] {
            assert!(!leaf(Kind::String, None, op, Some(&json!("a")), NOW));
            assert!(!leaf(
                Kind::String,
                Some(&Value::Null),
                op,
                Some(&json!("a")),
                NOW
            ));
        }
        assert!(leaf(Kind::String, None, Op::NotExists, None, NOW));
        assert!(leaf(
            Kind::Number,
            Some(&Value::Null),
            Op::NotExists,
            None,
            NOW
        ));
        assert!(!leaf(
            Kind::Number,
            Some(&json!(0)),
            Op::NotExists,
            None,
            NOW
        ));
    }

    #[test]
    fn string_equality_is_containment_so_the_type_must_match() {
        assert!(check(Kind::String, json!("5"), Op::Eq, Some(json!("5"))));
        assert!(!check(Kind::String, json!(5), Op::Eq, Some(json!("5"))));
        assert!(!check(Kind::String, json!(["5"]), Op::Eq, Some(json!("5"))));
        assert!(!check(Kind::Number, json!("5"), Op::Eq, Some(json!(5))));
        assert!(check(Kind::Boolean, json!(true), Op::Eq, Some(json!(true))));
        assert!(!check(
            Kind::Boolean,
            json!("true"),
            Op::Eq,
            Some(json!(true))
        ));
    }

    #[test]
    fn string_operators_need_a_string_and_the_empty_needle_matches() {
        let alpha = || json!("alpha");
        assert!(check(
            Kind::String,
            alpha(),
            Op::Contains,
            Some(json!("lph"))
        ));
        assert!(check(Kind::String, alpha(), Op::Contains, Some(json!(""))));
        assert!(check(
            Kind::String,
            alpha(),
            Op::StartsWith,
            Some(json!("al"))
        ));
        assert!(check(
            Kind::String,
            json!("a"),
            Op::NotIn,
            Some(json!(["b", "c"]))
        ));
        assert!(!check(
            Kind::String,
            json!(1),
            Op::NotIn,
            Some(json!(["b"]))
        ));
        assert!(!check(Kind::String, json!(1), Op::Neq, Some(json!("b"))));
    }

    #[test]
    fn numbers_past_the_bound_and_fractions_of_an_integer_are_missing() {
        assert!(check(Kind::Number, json!(1e300), Op::Gt, Some(json!(0))));
        let past = 1.000_000_000_000_000_2e300;
        assert!(!check(Kind::Number, json!(past), Op::Gt, Some(json!(0))));
        assert!(!check(Kind::Number, json!(-past), Op::Lt, Some(json!(0))));
        assert!(check(Kind::Integer, json!(5.0), Op::Eq, Some(json!(5))));
        assert!(!check(Kind::Integer, json!(5.5), Op::Neq, Some(json!(0))));
        assert!(!check(
            Kind::Integer,
            json!(5.5),
            Op::NotIn,
            Some(json!([0]))
        ));
        assert!(check(Kind::Number, json!(5.5), Op::Neq, Some(json!(0))));
    }

    #[test]
    fn number_between_is_inclusive_and_time_between_is_half_open() {
        assert!(check(
            Kind::Number,
            json!(1),
            Op::Between,
            Some(json!([1, 2]))
        ));
        assert!(check(
            Kind::Number,
            json!(2),
            Op::Between,
            Some(json!([1, 2]))
        ));
        let range = json!(["2025-06-15T00:00:00Z", "2025-06-16T00:00:00Z"]);
        let stamp = |text: &str| {
            check(
                Kind::Timestamp,
                json!(text),
                Op::Between,
                Some(range.clone()),
            )
        };
        assert!(stamp("2025-06-15T00:00:00Z"));
        assert!(!stamp("2025-06-16T00:00:00Z"));
        let days = json!(["2025-06-15", "2025-06-16"]);
        let day = |text: &str| check(Kind::Date, json!(text), Op::Between, Some(days.clone()));
        assert!(day("2025-06-15"));
        assert!(!day("2025-06-16"));
    }

    #[test]
    fn a_time_string_the_sql_refuses_is_missing() {
        let late = Some(json!("9999-01-01T00:00:00Z"));
        for text in [
            "infinity",
            "now",
            "2025-06-15",
            "0000-01-01T00:00:00Z",
            "2025-06-15T00:00:00+16:00",
        ] {
            assert!(
                !check(Kind::Timestamp, json!(text), Op::Before, late.clone()),
                "{text}"
            );
        }
        assert!(!check(
            Kind::Date,
            json!("2025-06-15T00:00:00Z"),
            Op::After,
            Some(json!("2000-01-01"))
        ));
    }

    #[test]
    fn relative_windows_have_their_own_edges() {
        let span = span_of("PT1H").unwrap();
        let hour = Some(json!("PT1H"));
        let run = |op: Op, at: i64| time(Some(at), Kind::Timestamp, op, hour.as_ref(), NOW);
        assert!(!run(Op::WithinLast, NOW - span));
        assert!(run(Op::WithinLast, NOW - span + 1));
        assert!(run(Op::WithinLast, NOW));
        assert!(!run(Op::WithinLast, NOW + 1));
        assert!(run(Op::MoreThanAgo, NOW - span));
        assert!(!run(Op::MoreThanAgo, NOW - span + 1));
        assert!(run(Op::WithinNext, NOW));
        assert!(!run(Op::WithinNext, NOW + span));
        assert!(!run(Op::WithinNext, NOW - 1));
    }

    #[test]
    fn list_operators_only_see_string_elements_of_an_array() {
        let stored = json!(["a", 1, "b"]);
        assert!(check(
            Kind::StringList,
            stored.clone(),
            Op::Contains,
            Some(json!(["z", "b"]))
        ));
        assert!(!check(
            Kind::StringList,
            stored.clone(),
            Op::ContainsAll,
            Some(json!(["a", "z"]))
        ));
        assert!(check(
            Kind::StringList,
            stored,
            Op::ContainsAll,
            Some(json!(["a", "b"]))
        ));
        assert!(check(Kind::StringList, json!([]), Op::IsEmpty, None));
        assert!(!check(Kind::StringList, json!("a"), Op::IsEmpty, None));
        assert!(!check(
            Kind::StringList,
            json!({"a": 1}),
            Op::Contains,
            Some(json!(["a"]))
        ));
    }

    #[test]
    fn an_object_property_only_has_presence() {
        assert!(check(Kind::Object, json!({}), Op::Exists, None));
        assert!(!check(Kind::Object, json!({}), Op::Eq, Some(json!({}))));
    }
}
