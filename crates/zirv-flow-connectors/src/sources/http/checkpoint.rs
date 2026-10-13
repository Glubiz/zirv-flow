//! The checkpoint of an `http` stream and the rule that advances it.
//!
//! The checkpoint is `{"since": <text|null>}`. An API may return records in any order, so the
//! checkpoint moves only at the end of the stream: every batch but the last carries the
//! checkpoint the run started with, and the last carries the highest cursor value seen. After a
//! lost lease the next run re-reads from its start checkpoint: that costs requests, not
//! correctness, because applying a record twice is idempotent through the ledger content hash.

use chrono::{DateTime, FixedOffset};
use serde_json::{Value, json};

use super::super::types::Checkpoint;
use crate::manifest::CursorType;

/// The `since` a checkpoint holds: where the previous run ended. Anything that is not the shape
/// this source writes starts the stream from the beginning.
pub(super) fn since_of(checkpoint: Option<&Checkpoint>) -> Option<String> {
    checkpoint?
        .0
        .get("since")?
        .as_str()
        .map(str::to_owned)
        .filter(|since| !since.is_empty())
}

pub(super) fn checkpoint(since: Option<&str>) -> Checkpoint {
    Checkpoint(json!({ "since": since }))
}

/// A comparable cursor value.
#[derive(Clone, Debug)]
enum Position {
    Instant(DateTime<FixedOffset>),
    Integer(i64),
}

impl Position {
    fn parse(cursor_type: CursorType, value: &Value) -> Option<Self> {
        match (cursor_type, value) {
            (CursorType::Timestamp, Value::String(text)) => {
                DateTime::parse_from_rfc3339(text).ok().map(Self::Instant)
            }
            (CursorType::Integer, Value::Number(number)) => number.as_i64().map(Self::Integer),
            (CursorType::Integer, Value::String(text)) => text.parse().ok().map(Self::Integer),
            _ => None,
        }
    }

    fn is_after(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Instant(a), Self::Instant(b)) => a > b,
            (Self::Integer(a), Self::Integer(b)) => a > b,
            _ => false,
        }
    }
}

/// The highest cursor value seen, starting from the checkpoint the run began with so the
/// checkpoint never moves back.
pub(super) struct Highest {
    cursor_type: CursorType,
    best: Option<(Position, String)>,
}

impl Highest {
    pub(super) fn new(cursor_type: CursorType, start: Option<&str>) -> Self {
        let best = start.and_then(|text| {
            let position = Position::parse(cursor_type, &Value::String(text.to_owned()))?;
            Some((position, text.to_owned()))
        });
        Self { cursor_type, best }
    }

    /// Looks at one record's cursor value. A missing or unparsable value is ignored: the record
    /// is still read, it just cannot move the checkpoint.
    pub(super) fn observe(&mut self, value: Option<&Value>) {
        let Some(value) = value else {
            return;
        };
        let Some(position) = Position::parse(self.cursor_type, value) else {
            return;
        };
        if self
            .best
            .as_ref()
            .is_some_and(|(best, _)| !position.is_after(best))
        {
            return;
        }
        let text = match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        self.best = Some((position, text));
    }

    pub(super) fn text(&self) -> Option<&str> {
        self.best.as_ref().map(|(_, text)| text.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_is_read_from_the_shape_this_source_writes_and_nothing_else() {
        assert_eq!(
            since_of(Some(&checkpoint(Some("2026-01-01T00:00:00Z")))).as_deref(),
            Some("2026-01-01T00:00:00Z")
        );
        for other in [
            None,
            Some(checkpoint(None)),
            Some(Checkpoint(json!({"since": 5}))),
            Some(Checkpoint(json!({"since": ""}))),
            Some(Checkpoint(json!("2026"))),
            Some(Checkpoint(json!({}))),
        ] {
            assert_eq!(since_of(other.as_ref()), None, "{other:?}");
        }
    }

    #[test]
    fn timestamps_are_compared_as_instants_not_as_text() {
        let mut highest = Highest::new(CursorType::Timestamp, None);
        // 10:00 +02:00 is 08:00Z, earlier than 09:00Z although it sorts later as text.
        highest.observe(Some(&json!("2026-01-01T09:00:00Z")));
        highest.observe(Some(&json!("2026-01-01T10:00:00+02:00")));
        assert_eq!(highest.text(), Some("2026-01-01T09:00:00Z"));

        highest.observe(Some(&json!("2026-01-01T09:30:00.5Z")));
        assert_eq!(highest.text(), Some("2026-01-01T09:30:00.5Z"));
    }

    #[test]
    fn integers_are_compared_as_numbers_and_unordered_input_gives_the_true_maximum() {
        let mut highest = Highest::new(CursorType::Integer, None);
        for value in [json!(9), json!(100), json!("20"), json!(3)] {
            highest.observe(Some(&value));
        }

        assert_eq!(highest.text(), Some("100"));
    }

    #[test]
    fn a_missing_or_unparsable_value_does_not_move_the_maximum() {
        let mut highest = Highest::new(CursorType::Timestamp, None);
        highest.observe(Some(&json!("2026-01-01T00:00:00Z")));
        for value in [json!("not a date"), json!(5), json!(null), json!({"a": 1})] {
            highest.observe(Some(&value));
        }
        highest.observe(None);

        assert_eq!(highest.text(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(Highest::new(CursorType::Integer, None).text(), None);
    }

    #[test]
    fn the_start_checkpoint_is_the_floor() {
        let mut highest = Highest::new(CursorType::Integer, Some("50"));
        highest.observe(Some(&json!(10)));
        assert_eq!(highest.text(), Some("50"));

        highest.observe(Some(&json!(51)));
        assert_eq!(highest.text(), Some("51"));
        assert_eq!(Highest::new(CursorType::Integer, Some("junk")).text(), None);
    }
}
