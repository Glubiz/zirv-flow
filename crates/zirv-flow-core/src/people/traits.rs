//! Trait keys, types, value validation and inference (architecture section 7.1).

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, NaiveDate, SecondsFormat};

use crate::ValidationError;

const MAX_STRING_BYTES: usize = 4096;
const MAX_LIST_ITEMS: usize = 100;
const MAX_LIST_ITEM_BYTES: usize = 256;

/// A trait key: `[a-z][a-z0-9_]{0,63}`.
pub fn validate_key(key: &str) -> Result<(), ValidationError> {
    let mut bytes = key.bytes();
    let valid = key.len() <= 64
        && bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
    if valid {
        Ok(())
    } else {
        Err(ValidationError::new(
            "key",
            "must start with a lowercase letter and use lowercase letters, digits and underscores (at most 64)",
        ))
    }
}

/// The type of a trait; fixed at creation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TraitType {
    String,
    Number,
    Boolean,
    Timestamp,
    Date,
    StringList,
}

impl TraitType {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Timestamp => "timestamp",
            Self::Date => "date",
            Self::StringList => "string_list",
        }
    }
}

impl fmt::Display for TraitType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for TraitType {
    type Err = ValidationError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "string" => Ok(Self::String),
            "number" => Ok(Self::Number),
            "boolean" => Ok(Self::Boolean),
            "timestamp" => Ok(Self::Timestamp),
            "date" => Ok(Self::Date),
            "string_list" => Ok(Self::StringList),
            _ => Err(ValidationError::new("type", "is not a known trait type")),
        }
    }
}

/// An incoming value, shaped like JSON without depending on a JSON library.
#[derive(Clone, Debug, PartialEq)]
pub enum RawValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<RawValue>),
    /// A JSON object, which no trait type accepts.
    Object,
}

/// A value that passed its type's rules, normalized.
#[derive(Clone, Debug, PartialEq)]
pub enum TraitValue {
    String(String),
    Number(f64),
    Boolean(bool),
    /// Normalized to UTC RFC 3339.
    Timestamp(String),
    /// `YYYY-MM-DD`.
    Date(String),
    StringList(Vec<String>),
}

/// Why a trait was dropped from a record. Static, so it is safe in decisions and answers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DropReason {
    InvalidKey,
    Archived,
    WrongType,
    TooLong,
    NotInferable,
}

impl DropReason {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::InvalidKey => "invalid_key",
            Self::Archived => "archived_key",
            Self::WrongType => "wrong_type",
            Self::TooLong => "too_long",
            Self::NotInferable => "not_inferable",
        }
    }
}

/// The type an unregistered key is registered with. Timestamp and date are never inferred.
pub fn infer_type(value: &RawValue) -> Result<TraitType, DropReason> {
    match value {
        RawValue::String(_) => Ok(TraitType::String),
        RawValue::Number(_) => Ok(TraitType::Number),
        RawValue::Bool(_) => Ok(TraitType::Boolean),
        RawValue::Array(items) if items.iter().all(|item| matches!(item, RawValue::String(_))) => {
            Ok(TraitType::StringList)
        }
        _ => Err(DropReason::NotInferable),
    }
}

/// Checks `value` against `kind` and normalizes it. `Null` is not a value; callers handle it.
pub fn coerce(kind: TraitType, value: &RawValue) -> Result<TraitValue, DropReason> {
    match (kind, value) {
        (TraitType::String, RawValue::String(text)) => {
            if text.len() > MAX_STRING_BYTES {
                Err(DropReason::TooLong)
            } else {
                Ok(TraitValue::String(text.clone()))
            }
        }
        (TraitType::Number, RawValue::Number(number)) if number.is_finite() => {
            Ok(TraitValue::Number(*number))
        }
        (TraitType::Boolean, RawValue::Bool(flag)) => Ok(TraitValue::Boolean(*flag)),
        (TraitType::Timestamp, RawValue::String(text)) => DateTime::parse_from_rfc3339(text)
            .map(|parsed| {
                TraitValue::Timestamp(parsed.to_utc().to_rfc3339_opts(SecondsFormat::AutoSi, true))
            })
            .map_err(|_| DropReason::WrongType),
        (TraitType::Date, RawValue::String(text)) => {
            let well_formed = text.len() == 10
                && text.bytes().enumerate().all(|(i, byte)| {
                    if i == 4 || i == 7 {
                        byte == b'-'
                    } else {
                        byte.is_ascii_digit()
                    }
                });
            if !well_formed {
                return Err(DropReason::WrongType);
            }
            NaiveDate::parse_from_str(text, "%Y-%m-%d")
                .map(|_| TraitValue::Date(text.clone()))
                .map_err(|_| DropReason::WrongType)
        }
        (TraitType::StringList, RawValue::Array(items)) => {
            if items.len() > MAX_LIST_ITEMS {
                return Err(DropReason::TooLong);
            }
            let mut texts = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    RawValue::String(text) if text.len() <= MAX_LIST_ITEM_BYTES => {
                        texts.push(text.clone());
                    }
                    RawValue::String(_) => return Err(DropReason::TooLong),
                    _ => return Err(DropReason::WrongType),
                }
            }
            Ok(TraitValue::StringList(texts))
        }
        _ => Err(DropReason::WrongType),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> RawValue {
        RawValue::String(value.to_owned())
    }

    #[test]
    fn keys_follow_the_syntax() {
        for good in ["a", "plan_tier", "x9", &"a".repeat(64)] {
            assert!(validate_key(good).is_ok(), "{good}");
        }
        for bad in ["", "A", "9a", "_a", "a-b", "a b", &"a".repeat(65)] {
            assert!(validate_key(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn types_round_trip_through_text() {
        for kind in [
            TraitType::String,
            TraitType::Number,
            TraitType::Boolean,
            TraitType::Timestamp,
            TraitType::Date,
            TraitType::StringList,
        ] {
            assert_eq!(kind.as_str().parse::<TraitType>().unwrap(), kind);
        }
        assert!("integer".parse::<TraitType>().is_err());
    }

    #[test]
    fn string_is_capped_at_4096_bytes() {
        assert!(coerce(TraitType::String, &text(&"a".repeat(4096))).is_ok());
        assert_eq!(
            coerce(TraitType::String, &text(&"a".repeat(4097))),
            Err(DropReason::TooLong)
        );
        assert_eq!(
            coerce(TraitType::String, &RawValue::Bool(true)),
            Err(DropReason::WrongType)
        );
    }

    #[test]
    fn number_must_be_finite() {
        assert_eq!(
            coerce(TraitType::Number, &RawValue::Number(1.5)),
            Ok(TraitValue::Number(1.5))
        );
        assert_eq!(
            coerce(TraitType::Number, &RawValue::Number(f64::NAN)),
            Err(DropReason::WrongType)
        );
        assert_eq!(
            coerce(TraitType::Number, &text("1")),
            Err(DropReason::WrongType)
        );
    }

    #[test]
    fn boolean_takes_only_booleans() {
        assert_eq!(
            coerce(TraitType::Boolean, &RawValue::Bool(false)),
            Ok(TraitValue::Boolean(false))
        );
        assert_eq!(
            coerce(TraitType::Boolean, &text("true")),
            Err(DropReason::WrongType)
        );
    }

    #[test]
    fn timestamp_is_normalized_to_utc() {
        assert_eq!(
            coerce(TraitType::Timestamp, &text("2026-05-01T12:00:00+02:00")),
            Ok(TraitValue::Timestamp("2026-05-01T10:00:00Z".into()))
        );
        assert_eq!(
            coerce(TraitType::Timestamp, &text("2026-05-01")),
            Err(DropReason::WrongType)
        );
    }

    #[test]
    fn date_is_strict() {
        assert_eq!(
            coerce(TraitType::Date, &text("2026-02-28")),
            Ok(TraitValue::Date("2026-02-28".into()))
        );
        for bad in [
            "2026-02-30",
            "2026-2-3",
            "26-02-03",
            "2026/02/03",
            "2026-02-03T00:00:00Z",
        ] {
            assert_eq!(
                coerce(TraitType::Date, &text(bad)),
                Err(DropReason::WrongType),
                "{bad}"
            );
        }
    }

    #[test]
    fn string_list_is_bounded() {
        let list = |n: usize, len: usize| {
            RawValue::Array((0..n).map(|_| text(&"a".repeat(len))).collect())
        };
        assert!(coerce(TraitType::StringList, &list(100, 256)).is_ok());
        assert_eq!(
            coerce(TraitType::StringList, &list(101, 1)),
            Err(DropReason::TooLong)
        );
        assert_eq!(
            coerce(TraitType::StringList, &list(1, 257)),
            Err(DropReason::TooLong)
        );
        assert_eq!(
            coerce(
                TraitType::StringList,
                &RawValue::Array(vec![RawValue::Bool(true)])
            ),
            Err(DropReason::WrongType)
        );
    }

    #[test]
    fn inference_picks_the_plain_types_and_never_timestamp_or_date() {
        assert_eq!(infer_type(&text("2026-05-01")), Ok(TraitType::String));
        assert_eq!(infer_type(&RawValue::Number(1.0)), Ok(TraitType::Number));
        assert_eq!(infer_type(&RawValue::Bool(true)), Ok(TraitType::Boolean));
        assert_eq!(
            infer_type(&RawValue::Array(vec![text("a")])),
            Ok(TraitType::StringList)
        );
        assert_eq!(
            infer_type(&RawValue::Array(vec![])),
            Ok(TraitType::StringList)
        );
    }

    #[test]
    fn objects_and_mixed_arrays_are_not_inferable() {
        assert_eq!(infer_type(&RawValue::Object), Err(DropReason::NotInferable));
        assert_eq!(
            infer_type(&RawValue::Array(vec![text("a"), RawValue::Number(1.0)])),
            Err(DropReason::NotInferable)
        );
        assert_eq!(infer_type(&RawValue::Null), Err(DropReason::NotInferable));
    }
}
