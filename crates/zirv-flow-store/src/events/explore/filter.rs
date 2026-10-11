//! The explorer's property filters (architecture section 12.3): which operators a property
//! type takes, and how a request value becomes a typed test. Pure: nothing here touches the
//! database. A refusal names the field and the rule, never the value.

use std::str::FromStr;

use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::{Date, Month, OffsetDateTime};
use zirv_flow_core::events::PropertyType;

const MAX_KEY_CHARS: usize = 128;
const MAX_IN_VALUES: usize = 100;

/// A filter operator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilterOp {
    /// For a string list, "contains".
    Eq,
    /// The negation of `Eq`: an event without the property matches.
    Ne,
    In,
    Prefix,
    Gt,
    Gte,
    Lt,
    Lte,
    Exists,
}

impl FilterOp {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::Ne => "ne",
            Self::In => "in",
            Self::Prefix => "prefix",
            Self::Gt => "gt",
            Self::Gte => "gte",
            Self::Lt => "lt",
            Self::Lte => "lte",
            Self::Exists => "exists",
        }
    }

    /// Whether the operator takes a property of `kind`; an undeclared property (`None`) is
    /// compared as text.
    pub fn allows(self, kind: Option<PropertyType>) -> bool {
        use PropertyType as T;
        match (self, kind) {
            (Self::Exists, _) => true,
            (Self::Eq | Self::Ne, Some(T::Object)) => false,
            (Self::Eq | Self::Ne, _) => true,
            (Self::In, None | Some(T::String | T::Number | T::Integer)) => true,
            (Self::Prefix, None | Some(T::String)) => true,
            (
                Self::Gt | Self::Gte | Self::Lt | Self::Lte,
                None | Some(T::Number | T::Integer | T::Timestamp | T::Date),
            ) => true,
            _ => false,
        }
    }

    /// Operators that reveal nothing about a value the asker does not already know.
    pub const fn is_exact(self) -> bool {
        matches!(self, Self::Eq | Self::Exists)
    }
}

impl FromStr for FilterOp {
    type Err = ();

    fn from_str(text: &str) -> Result<Self, ()> {
        match text {
            "eq" => Ok(Self::Eq),
            "ne" => Ok(Self::Ne),
            "in" => Ok(Self::In),
            "prefix" => Ok(Self::Prefix),
            "gt" => Ok(Self::Gt),
            "gte" => Ok(Self::Gte),
            "lt" => Ok(Self::Lt),
            "lte" => Ok(Self::Lte),
            "exists" => Ok(Self::Exists),
            _ => Err(()),
        }
    }
}

/// One filter as a caller gives it.
#[derive(Clone, Debug, PartialEq)]
pub struct ExploreFilter {
    /// A top-level property name.
    pub key: String,
    pub op: FilterOp,
    /// Absent for `exists`, which then means "the property is present"; `false` means absent.
    pub value: Option<Value>,
}

/// How a typed value is compared.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Compare {
    Eq,
    Gt,
    Gte,
    Lt,
    Lte,
}

impl Compare {
    pub(super) const fn sql(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Gt => ">",
            Self::Gte => ">=",
            Self::Lt => "<",
            Self::Lte => "<=",
        }
    }
}

/// What a filter tests, with its value already typed.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Test {
    Exists(bool),
    Text(Compare, String),
    TextIn(Vec<String>),
    Prefix(String),
    Numeric(Compare, String),
    NumericIn(Vec<String>),
    Bool(bool),
    Timestamp(Compare, OffsetDateTime),
    /// A string list holds the text.
    Contains(String),
}

/// A filter ready to become SQL.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Planned {
    pub(super) key: String,
    pub(super) test: Test,
    /// `ne`: the test must not hold, and an event without the property passes.
    pub(super) negate: bool,
}

/// Why a filter was refused: the field and the rule.
pub(super) type Refusal = (&'static str, &'static str);

fn refuse<T>(field: &'static str, reason: &'static str) -> Result<T, Refusal> {
    Err((field, reason))
}

/// Checks the key of a filter or a breakdown property.
pub(super) fn check_key(field: &'static str, key: &str) -> Result<(), Refusal> {
    let length = key.chars().count();
    if (1..=MAX_KEY_CHARS).contains(&length) && !key.chars().any(char::is_control) {
        Ok(())
    } else {
        refuse(
            field,
            "must be 1 to 128 characters without control characters",
        )
    }
}

/// Types `filter` for a property declared as `kind`, or compared as text when undeclared.
pub(super) fn plan(filter: &ExploreFilter, kind: Option<PropertyType>) -> Result<Planned, Refusal> {
    use PropertyType as T;

    check_key("filters.key", &filter.key)?;
    if !filter.op.allows(kind) {
        return refuse("filters.op", "does not apply to the type of this property");
    }
    let key = filter.key.clone();
    let planned = |test, negate| Planned {
        key: key.clone(),
        test,
        negate,
    };
    let wrong = || -> Result<Planned, Refusal> {
        refuse("filters.value", "does not fit the type of this property")
    };
    if filter.op == FilterOp::Exists {
        return match &filter.value {
            None => Ok(planned(Test::Exists(true), false)),
            Some(Value::Bool(present)) => Ok(planned(Test::Exists(*present), false)),
            Some(_) => refuse("filters.value", "must be true or false for exists"),
        };
    }
    let Some(value) = &filter.value else {
        return refuse("filters.value", "is required for this operator");
    };
    let negate = filter.op == FilterOp::Ne;
    let compare = match filter.op {
        FilterOp::Gt => Compare::Gt,
        FilterOp::Gte => Compare::Gte,
        FilterOp::Lt => Compare::Lt,
        FilterOp::Lte => Compare::Lte,
        _ => Compare::Eq,
    };
    if filter.op == FilterOp::In {
        let Value::Array(items) = value else {
            return refuse("filters.value", "must be a list for in");
        };
        if items.is_empty() || items.len() > MAX_IN_VALUES {
            return refuse("filters.value", "must hold 1 to 100 values for in");
        }
        return match kind {
            Some(T::Number | T::Integer) => items
                .iter()
                .map(numeric)
                .collect::<Option<Vec<_>>>()
                .map_or_else(wrong, |list| Ok(planned(Test::NumericIn(list), false))),
            _ => items
                .iter()
                .map(|item| text(item, kind.is_none()))
                .collect::<Option<Vec<_>>>()
                .map_or_else(wrong, |list| Ok(planned(Test::TextIn(list), false))),
        };
    }
    match kind {
        Some(T::Number | T::Integer) => match numeric(value) {
            Some(number) => Ok(planned(Test::Numeric(compare, number), negate)),
            None => wrong(),
        },
        Some(T::Boolean) => match value {
            Value::Bool(flag) => Ok(planned(Test::Bool(*flag), negate)),
            _ => wrong(),
        },
        Some(T::Timestamp) => match value
            .as_str()
            .and_then(|text| OffsetDateTime::parse(text, &Rfc3339).ok())
        {
            Some(at) => Ok(planned(Test::Timestamp(compare, at), negate)),
            None => wrong(),
        },
        Some(T::Date) => match value.as_str().filter(|text| is_date(text)) {
            Some(date) => Ok(planned(Test::Text(compare, date.to_owned()), negate)),
            None => wrong(),
        },
        Some(T::StringList) => match value {
            Value::String(item) => Ok(planned(Test::Contains(item.clone()), negate)),
            _ => wrong(),
        },
        Some(T::Object) => wrong(),
        Some(T::String) | None => match text(value, kind.is_none()) {
            Some(text) if filter.op == FilterOp::Prefix => Ok(planned(Test::Prefix(text), false)),
            Some(text) => Ok(planned(Test::Text(compare, text), negate)),
            None => wrong(),
        },
    }
}

/// A number as the text PostgreSQL reads as `numeric`.
fn numeric(value: &Value) -> Option<String> {
    match value {
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// A string; for an undeclared property also a number or a boolean, as its text.
fn text(value: &Value, lenient: bool) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) if lenient => Some(number.to_string()),
        Value::Bool(flag) if lenient => Some(flag.to_string()),
        _ => None,
    }
}

fn is_date(text: &str) -> bool {
    let mut parts = text.split('-');
    let (Some(year), Some(month), Some(day), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    if year.len() != 4 || month.len() != 2 || day.len() != 2 {
        return false;
    }
    let (Ok(year), Ok(month), Ok(day)) = (year.parse(), month.parse::<u8>(), day.parse()) else {
        return false;
    };
    Month::try_from(month)
        .ok()
        .is_some_and(|month| Date::from_calendar_date(year, month, day).is_ok())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn filter(key: &str, op: FilterOp, value: Option<Value>) -> ExploreFilter {
        ExploreFilter {
            key: key.to_owned(),
            op,
            value,
        }
    }

    #[test]
    fn operators_follow_the_declared_type() {
        use PropertyType as T;
        assert!(FilterOp::Prefix.allows(Some(T::String)));
        assert!(!FilterOp::Prefix.allows(Some(T::Number)));
        assert!(FilterOp::Gt.allows(Some(T::Integer)));
        assert!(FilterOp::Lte.allows(Some(T::Timestamp)));
        assert!(!FilterOp::Gt.allows(Some(T::String)));
        assert!(!FilterOp::Gt.allows(Some(T::Boolean)));
        assert!(FilterOp::Eq.allows(Some(T::Boolean)));
        assert!(!FilterOp::Eq.allows(Some(T::Object)));
        assert!(FilterOp::Exists.allows(Some(T::Object)));
        for op in [FilterOp::Eq, FilterOp::Prefix, FilterOp::Gte, FilterOp::In] {
            assert!(op.allows(None), "{op:?}");
        }
    }

    #[test]
    fn an_operator_name_round_trips() {
        for op in [
            FilterOp::Eq,
            FilterOp::Ne,
            FilterOp::In,
            FilterOp::Prefix,
            FilterOp::Gt,
            FilterOp::Gte,
            FilterOp::Lt,
            FilterOp::Lte,
            FilterOp::Exists,
        ] {
            assert_eq!(op.as_str().parse(), Ok(op));
        }
        assert_eq!("like".parse::<FilterOp>(), Err(()));
    }

    #[test]
    fn a_number_filter_takes_numbers_only() {
        let ok = plan(
            &filter("total", FilterOp::Gte, Some(json!(12.5))),
            Some(PropertyType::Number),
        )
        .unwrap();
        let bad = plan(
            &filter("total", FilterOp::Gte, Some(json!("12"))),
            Some(PropertyType::Number),
        );

        assert_eq!(ok.test, Test::Numeric(Compare::Gte, "12.5".to_owned()));
        assert_eq!(bad.unwrap_err().0, "filters.value");
    }

    #[test]
    fn ne_is_the_negation_of_eq() {
        let planned = plan(
            &filter("plan", FilterOp::Ne, Some(json!("pro"))),
            Some(PropertyType::String),
        )
        .unwrap();

        assert_eq!(planned.test, Test::Text(Compare::Eq, "pro".to_owned()));
        assert!(planned.negate);
    }

    #[test]
    fn an_undeclared_property_compares_as_text() {
        let planned = plan(&filter("n", FilterOp::Eq, Some(json!(5))), None).unwrap();
        let ranged = plan(&filter("n", FilterOp::Gt, Some(json!("b"))), None).unwrap();
        let listed = plan(&filter("n", FilterOp::In, Some(json!(["a", 2]))), None).unwrap();

        assert_eq!(planned.test, Test::Text(Compare::Eq, "5".to_owned()));
        assert_eq!(ranged.test, Test::Text(Compare::Gt, "b".to_owned()));
        assert_eq!(
            listed.test,
            Test::TextIn(vec!["a".to_owned(), "2".to_owned()])
        );
    }

    #[test]
    fn exists_takes_no_value_or_a_boolean() {
        let none = plan(&filter("a", FilterOp::Exists, None), None).unwrap();
        let absent = plan(&filter("a", FilterOp::Exists, Some(json!(false))), None).unwrap();
        let bad = plan(&filter("a", FilterOp::Exists, Some(json!("x"))), None);

        assert_eq!(none.test, Test::Exists(true));
        assert_eq!(absent.test, Test::Exists(false));
        assert!(bad.is_err());
    }

    #[test]
    fn timestamps_dates_lists_and_in_are_typed() {
        let at = plan(
            &filter("t", FilterOp::Lt, Some(json!("2026-06-01T12:00:00+02:00"))),
            Some(PropertyType::Timestamp),
        )
        .unwrap();
        let date = plan(
            &filter("d", FilterOp::Gt, Some(json!("2026-06-01"))),
            Some(PropertyType::Date),
        )
        .unwrap();
        let contains = plan(
            &filter("tags", FilterOp::Eq, Some(json!("vip"))),
            Some(PropertyType::StringList),
        )
        .unwrap();
        let in_numbers = plan(
            &filter("n", FilterOp::In, Some(json!([1, 2]))),
            Some(PropertyType::Integer),
        )
        .unwrap();

        assert!(matches!(at.test, Test::Timestamp(Compare::Lt, _)));
        assert_eq!(date.test, Test::Text(Compare::Gt, "2026-06-01".to_owned()));
        assert_eq!(contains.test, Test::Contains("vip".to_owned()));
        assert_eq!(
            in_numbers.test,
            Test::NumericIn(vec!["1".to_owned(), "2".to_owned()])
        );
    }

    #[test]
    fn bad_values_keys_and_lists_are_refused_without_echoing_them() {
        let cases = [
            filter("d", FilterOp::Eq, Some(json!("2026-13-01"))),
            filter("", FilterOp::Eq, Some(json!("x"))),
            filter("a", FilterOp::In, Some(json!("x"))),
            filter("a", FilterOp::In, Some(json!([]))),
            filter("a", FilterOp::Eq, None),
        ];
        for case in cases {
            let kind = (case.key == "d").then_some(PropertyType::Date);
            let (field, reason) = plan(&case, kind).unwrap_err();
            assert!(field.starts_with("filters."));
            assert!(!reason.contains("2026"));
        }
        let boolean = filter("a", FilterOp::Eq, Some(json!(true)));
        assert!(plan(&boolean, Some(PropertyType::Boolean)).is_ok());
    }
}
