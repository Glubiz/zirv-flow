//! Typed slot columns on `people` (architecture sections 6 and 21.1): the fixed set of
//! columns a hot trait can be promoted to, and the one SQL conversion from the `jsonb` value
//! to a slot's value. The compiler reads a slot with the same guards the `jsonb` path uses, and
//! the write path and the backfill fill it with the same expression, so the two agree.
//!
//! A slot name reaches SQL only through [`SlotColumn::as_str`], never from a user string.

use std::fmt;
use std::str::FromStr;

use super::TraitType;

/// The kind of value a slot holds.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SlotKind {
    String,
    Number,
    Timestamp,
    Date,
    Boolean,
}

impl SlotKind {
    /// The slot kind of a trait type, or `None` for a type that stays `jsonb`.
    pub const fn of(trait_type: TraitType) -> Option<Self> {
        match trait_type {
            TraitType::String => Some(Self::String),
            TraitType::Number => Some(Self::Number),
            TraitType::Boolean => Some(Self::Boolean),
            TraitType::Timestamp => Some(Self::Timestamp),
            TraitType::Date => Some(Self::Date),
            TraitType::StringList => None,
        }
    }

    /// How many slots of this kind a workspace has.
    pub const fn capacity(self) -> usize {
        match self {
            Self::String | Self::Number => 8,
            Self::Timestamp => 6,
            Self::Date | Self::Boolean => 4,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Number => "number",
            Self::Timestamp => "timestamp",
            Self::Date => "date",
            Self::Boolean => "boolean",
        }
    }
}

macro_rules! slot_columns {
    ($($variant:ident $name:literal $kind:ident),* $(,)?) => {
        /// One of the 30 slot columns of `people`. The enum is the allowlist of column names.
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub enum SlotColumn {
            $($variant),*
        }

        impl SlotColumn {
            /// Every slot, in the order a workspace assigns them (lowest free first).
            pub const ALL: [SlotColumn; 30] = [$(SlotColumn::$variant),*];

            /// The column name.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $name),*
                }
            }

            pub const fn kind(self) -> SlotKind {
                match self {
                    $(Self::$variant => SlotKind::$kind),*
                }
            }
        }
    };
}

slot_columns! {
    S1 "trait_s1" String, S2 "trait_s2" String, S3 "trait_s3" String, S4 "trait_s4" String,
    S5 "trait_s5" String, S6 "trait_s6" String, S7 "trait_s7" String, S8 "trait_s8" String,
    N1 "trait_n1" Number, N2 "trait_n2" Number, N3 "trait_n3" Number, N4 "trait_n4" Number,
    N5 "trait_n5" Number, N6 "trait_n6" Number, N7 "trait_n7" Number, N8 "trait_n8" Number,
    T1 "trait_t1" Timestamp, T2 "trait_t2" Timestamp, T3 "trait_t3" Timestamp,
    T4 "trait_t4" Timestamp, T5 "trait_t5" Timestamp, T6 "trait_t6" Timestamp,
    D1 "trait_d1" Date, D2 "trait_d2" Date, D3 "trait_d3" Date, D4 "trait_d4" Date,
    B1 "trait_b1" Boolean, B2 "trait_b2" Boolean, B3 "trait_b3" Boolean, B4 "trait_b4" Boolean,
}

impl SlotColumn {
    /// The slots of one kind, lowest first.
    pub fn of_kind(kind: SlotKind) -> impl Iterator<Item = SlotColumn> {
        Self::ALL
            .into_iter()
            .filter(move |slot| slot.kind() == kind)
    }

    /// The SQL expression that computes this slot's value from a `jsonb` object and a key,
    /// each given as SQL text (`traits_sql` is an object, `key_sql` a `text` value).
    pub fn value_sql(self, traits_sql: &str, key_sql: &str) -> String {
        let json = format!("({traits_sql} -> {key_sql})");
        let text = format!("({traits_sql} ->> {key_sql})");
        match self.kind() {
            SlotKind::String => string_sql(&json, &text),
            SlotKind::Number => number_sql(&json, false),
            SlotKind::Boolean => boolean_sql(&json),
            SlotKind::Timestamp => time_sql(&json, &text, false),
            SlotKind::Date => time_sql(&json, &text, true),
        }
    }
}

impl fmt::Display for SlotColumn {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A text that is not a slot column name or a slot state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnknownSlot;

impl FromStr for SlotColumn {
    type Err = UnknownSlot;

    fn from_str(text: &str) -> Result<Self, UnknownSlot> {
        Self::ALL
            .into_iter()
            .find(|slot| slot.as_str() == text)
            .ok_or(UnknownSlot)
    }
}

/// Where a slot is in its life. Only `Ready` is read by the compiler; `Backfilling` and `Ready`
/// are written by every trait writer; `Retiring` is neither read nor written.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlotState {
    Backfilling,
    Ready,
    Retiring,
}

impl SlotState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Backfilling => "backfilling",
            Self::Ready => "ready",
            Self::Retiring => "retiring",
        }
    }
}

impl FromStr for SlotState {
    type Err = UnknownSlot;

    fn from_str(text: &str) -> Result<Self, UnknownSlot> {
        [Self::Backfilling, Self::Ready, Self::Retiring]
            .into_iter()
            .find(|state| state.as_str() == text)
            .ok_or(UnknownSlot)
    }
}

/// The RFC 3339 shape the write path accepts. PostgreSQL alone also takes `infinity`, `now` and
/// other spellings the write path refuses.
pub const TIMESTAMP_SHAPE: &str = "^[0-9]{4}-[0-9]{2}-[0-9]{2}[Tt ][0-9]{2}:[0-9]{2}:[0-9]{2}([.][0-9]+)?([Zz]|[+-][0-9]{2}:[0-9]{2})$";
pub const DATE_SHAPE: &str = "^[0-9]{4}-[0-9]{2}-[0-9]{2}$";

/// `json` as text when it is a JSON string, else NULL.
pub fn string_sql(json: &str, text: &str) -> String {
    format!("CASE WHEN jsonb_typeof({json}) = 'string' THEN {text} END")
}

/// `json` as `boolean` when it is a JSON boolean, else NULL.
pub fn boolean_sql(json: &str) -> String {
    format!("CASE WHEN jsonb_typeof({json}) = 'boolean' THEN ({json})::boolean END")
}

/// The value as `float8`, or NULL when it is not a number, its magnitude is past `1e300`, or
/// `integral` is set and it has a fractional part. The nesting keeps the cast from running on a
/// wrong type.
pub fn number_sql(json: &str, integral: bool) -> String {
    let whole = if integral {
        format!(" AND ({json})::numeric = trunc(({json})::numeric)")
    } else {
        String::new()
    };
    format!(
        "CASE WHEN jsonb_typeof({json}) = 'number' THEN \
         CASE WHEN abs(({json})::numeric) <= 1e300{whole} THEN ({json})::float8 END END"
    )
}

/// The value as `timestamptz`, or NULL when it is not a string in the write path's shape that
/// PostgreSQL also accepts. A date is taken at UTC midnight.
pub fn time_sql(json: &str, text: &str, date: bool) -> String {
    let (cast, sql_type, shape) = if date {
        (
            format!("(({text})::date::timestamp AT TIME ZONE 'UTC')"),
            "date",
            DATE_SHAPE,
        )
    } else {
        (
            format!("({text})::timestamptz"),
            "timestamptz",
            TIMESTAMP_SHAPE,
        )
    };
    format!(
        "CASE WHEN jsonb_typeof({json}) = 'string' THEN \
         CASE WHEN {text} ~ '{shape}' AND pg_input_is_valid({text}, '{sql_type}') THEN {cast} END END"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_capacities_match_the_ruling() {
        let count = |kind| SlotColumn::of_kind(kind).count();
        for (kind, capacity) in [
            (SlotKind::String, 8),
            (SlotKind::Number, 8),
            (SlotKind::Timestamp, 6),
            (SlotKind::Date, 4),
            (SlotKind::Boolean, 4),
        ] {
            assert_eq!(count(kind), capacity);
            assert_eq!(kind.capacity(), capacity);
        }
    }

    #[test]
    fn a_string_list_has_no_slot_kind() {
        assert_eq!(SlotKind::of(TraitType::StringList), None);
        assert_eq!(SlotKind::of(TraitType::Date), Some(SlotKind::Date));
    }

    #[test]
    fn every_name_round_trips_and_nothing_else_parses() {
        for slot in SlotColumn::ALL {
            assert_eq!(slot.as_str().parse::<SlotColumn>(), Ok(slot));
        }
        assert!("trait_s9".parse::<SlotColumn>().is_err());
        assert!("traits".parse::<SlotColumn>().is_err());
        assert!("trait_s1; DROP TABLE people".parse::<SlotColumn>().is_err());
    }

    #[test]
    fn the_names_are_unique_and_each_starts_with_trait() {
        let mut names: Vec<&str> = SlotColumn::ALL.iter().map(|slot| slot.as_str()).collect();
        assert!(names.iter().all(|name| name.starts_with("trait_")));
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 30);
    }

    #[test]
    fn the_states_round_trip() {
        for state in [
            SlotState::Backfilling,
            SlotState::Ready,
            SlotState::Retiring,
        ] {
            assert_eq!(state.as_str().parse::<SlotState>(), Ok(state));
        }
        assert!("done".parse::<SlotState>().is_err());
    }

    #[test]
    fn the_conversion_guards_the_type_before_the_cast() {
        let number = SlotColumn::N1.value_sql("$3::jsonb", "$5::text");
        assert!(number.starts_with("CASE WHEN jsonb_typeof(($3::jsonb -> $5::text)) = 'number'"));
        assert!(number.contains("<= 1e300"));
        let date = SlotColumn::D1.value_sql("t", "k");
        assert!(date.contains("AT TIME ZONE 'UTC'") && date.contains("pg_input_is_valid"));
        let flag = SlotColumn::B1.value_sql("t", "k");
        assert!(flag.contains("= 'boolean' THEN ((t -> k))::boolean"));
    }
}
