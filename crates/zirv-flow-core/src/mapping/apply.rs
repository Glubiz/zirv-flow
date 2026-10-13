//! Applying a mapping to one row: a profile record's parts, or the codes of what is wrong.

use std::collections::HashMap;

use serde_json::{Map, Value};
use time::OffsetDateTime;
use uuid::Uuid;

use super::json_record::JsonRecord;
use super::model::{EventRule, Mapping, Target, Transform};
use super::transform::{apply_transform, cast, into_json, parse_flag, sql_timestamp};
use super::validate::{TraitCatalog, TraitState};
use crate::people::{Identifier, TraitType, TraitValue, coerce};

/// The longest record key a row may bring.
pub const MAX_KEY_CHARS: usize = 512;

/// Why a row was refused. Static codes, safe in answers and stored rejects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RowErrorReason {
    /// No identifier column held a value.
    MissingIdentifier,
    /// An identifier broke its kind's rules (a malformed email, for instance).
    InvalidIdentifier,
    /// A value does not fit the type of its trait or list column.
    WrongType,
    /// A timestamp could not be read with its format.
    BadTimestamp,
    /// The mapping names a column the row does not have.
    UnknownColumn,
    /// The key column's value is longer than 512 characters.
    InvalidKey,
}

impl RowErrorReason {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::MissingIdentifier => "missing_identifier",
            Self::InvalidIdentifier => "invalid_identifier",
            Self::WrongType => "wrong_type",
            Self::BadTimestamp => "bad_timestamp",
            Self::UnknownColumn => "unknown_column",
            Self::InvalidKey => "invalid_key",
        }
    }
}

/// One reason a row was refused, and the column it concerns. Never the value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RowError {
    pub column: String,
    pub reason: RowErrorReason,
}

/// The parts of a profile record a row yields.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Mapped {
    /// `(kind, value)`, normalized, in column order, without repeats.
    pub identifiers: Vec<(String, String)>,
    /// Trait key to value. A cell that is empty leaves its trait out.
    pub traits: Map<String, Value>,
    /// The lists the row's person joins.
    pub lists: Vec<Uuid>,
    /// The key column's value, when the mapping names one and the cell holds a value.
    pub key: Option<String>,
    /// The event the row yields, when the mapping describes one.
    pub event: Option<MappedEvent>,
}

/// The event a row yields.
#[derive(Clone, Debug, PartialEq)]
pub struct MappedEvent {
    pub name: String,
    /// When the event happened, when the mapping says; otherwise the caller decides.
    pub occurred_at: Option<OffsetDateTime>,
    /// Property name to value. A cell that is empty leaves its property out.
    pub properties: Map<String, Value>,
}

/// A row, seen by column name.
pub trait Cells {
    /// The cell of `column`: empty text when the row is shorter than the header, and none
    /// when no such column exists.
    fn cell(&self, column: &str) -> Option<&str>;
}

/// Why a header was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeaderError {
    /// A column name is empty after trimming.
    Blank,
    /// Two columns share a name.
    Duplicate,
}

impl HeaderError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Blank => "blank_header",
            Self::Duplicate => "duplicate_header",
        }
    }
}

/// The column names of a file, each unique.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Header {
    names: Vec<String>,
    index: HashMap<String, usize>,
}

impl Header {
    pub fn new(names: Vec<String>) -> Result<Self, HeaderError> {
        let mut index = HashMap::with_capacity(names.len());
        for (position, name) in names.iter().enumerate() {
            if name.trim().is_empty() {
                return Err(HeaderError::Blank);
            }
            if index.insert(name.clone(), position).is_some() {
                return Err(HeaderError::Duplicate);
            }
        }
        Ok(Self { names, index })
    }

    pub fn names(&self) -> &[String] {
        &self.names
    }

    pub fn contains(&self, column: &str) -> bool {
        self.index.contains_key(column)
    }
}

/// One data row of a file under its header.
pub struct CsvRow<'a> {
    pub header: &'a Header,
    pub cells: &'a [String],
}

impl Cells for CsvRow<'_> {
    fn cell(&self, column: &str) -> Option<&str> {
        let position = *self.header.index.get(column)?;
        Some(self.cells.get(position).map_or("", String::as_str))
    }
}

/// The text of a finished value, or none when it is empty (an absent value).
fn present(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => {
            let text = text.trim();
            (!text.is_empty()).then(|| text.to_owned())
        }
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// Runs the rules of `mapping` over `row`. Every problem in the row is reported, not only
/// the first.
pub fn apply(
    mapping: &Mapping,
    row: &dyn Cells,
    catalog: &dyn TraitCatalog,
) -> Result<Mapped, Vec<RowError>> {
    let mut mapped = Mapped::default();
    let mut errors = Vec::new();
    let mut first_identifier_column: Option<&str> = None;

    for rule in &mapping.columns {
        if matches!(rule.target, Target::Ignore) {
            continue;
        }
        let Some(cell) = row.cell(&rule.column) else {
            errors.push(RowError {
                column: rule.column.clone(),
                reason: RowErrorReason::UnknownColumn,
            });
            continue;
        };
        if let Target::Identifier { .. } = rule.target {
            first_identifier_column.get_or_insert(&rule.column);
        }
        let value = match run_chain(&rule.column, &rule.transforms, cell, row) {
            Ok(value) => value,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        let outcome = match &rule.target {
            Target::Identifier { kind } => identifier(kind, &value, &mut mapped),
            Target::Trait { key } => trait_value(key, value, catalog, &mut mapped),
            Target::List { list_id } => list_flag(*list_id, &value, &mut mapped),
            Target::Ignore => Ok(()),
        };
        if let Err(reason) = outcome {
            errors.push(RowError {
                column: rule.column.clone(),
                reason,
            });
        }
    }

    if let Some(column) = &mapping.key_column {
        match row.cell(column) {
            None => errors.push(RowError {
                column: column.clone(),
                reason: RowErrorReason::UnknownColumn,
            }),
            Some(cell) => {
                let key = cell.trim();
                if key.chars().count() > MAX_KEY_CHARS {
                    errors.push(RowError {
                        column: column.clone(),
                        reason: RowErrorReason::InvalidKey,
                    });
                } else if !key.is_empty() {
                    mapped.key = Some(key.to_owned());
                }
            }
        }
    }

    if let Some(rule) = &mapping.event {
        mapped.event = event(rule, row, &mut errors);
    }

    if mapped.identifiers.is_empty() && errors.is_empty() {
        errors.push(RowError {
            column: first_identifier_column.unwrap_or_default().to_owned(),
            reason: RowErrorReason::MissingIdentifier,
        });
    }
    if errors.is_empty() {
        Ok(mapped)
    } else {
        Err(errors)
    }
}

/// Maps one JSON record of a stream: reads it as cells (dotted paths included) and applies
/// `mapping`. This is the one function a sync run and the preview both call, so what the
/// preview shows is what the run stores. `declared` names the fields the stream's descriptor
/// lists.
pub fn map_json<'a>(
    mapping: &Mapping,
    data: &Value,
    declared: impl IntoIterator<Item = &'a str>,
    catalog: &dyn TraitCatalog,
) -> Result<Mapped, Vec<RowError>> {
    let record = JsonRecord::from_value_for_mapping(data, mapping, declared);
    apply(mapping, &record, catalog)
}

/// A cell after the transforms of its rule.
fn run_chain(
    column: &str,
    transforms: &[Transform],
    cell: &str,
    row: &dyn Cells,
) -> Result<Value, RowError> {
    let mut value = Value::String(cell.to_owned());
    for transform in transforms {
        value = apply_transform(transform, value, row, column)
            .map_err(|(column, reason)| RowError { column, reason })?;
    }
    Ok(value)
}

/// The most properties an event rule may name.
pub const MAX_EVENT_PROPERTIES: usize = 50;

/// The event of one row: its timestamp and properties, or the errors added to `errors`.
fn event(rule: &EventRule, row: &dyn Cells, errors: &mut Vec<RowError>) -> Option<MappedEvent> {
    let before = errors.len();
    let mut occurred_at = None;
    if let Some(timestamp) = &rule.timestamp {
        match chain_of(&timestamp.column, &timestamp.transforms, row) {
            Ok(value) if present(&value).is_some() => match instant(&value) {
                Some(at) => occurred_at = Some(at),
                None => errors.push(RowError {
                    column: timestamp.column.clone(),
                    reason: RowErrorReason::BadTimestamp,
                }),
            },
            Ok(_) => {}
            Err(error) => errors.push(error),
        }
    }
    let mut properties = Map::new();
    for property in &rule.properties {
        match chain_of(&property.column, &property.transforms, row) {
            Ok(value) => {
                if present(&value).is_some() {
                    properties.insert(property.property.clone(), value);
                }
            }
            Err(error) => errors.push(error),
        }
    }
    (errors.len() == before).then(|| MappedEvent {
        name: rule.name.clone(),
        occurred_at,
        properties,
    })
}

/// The value of `column` after `transforms`; the column must exist.
fn chain_of(column: &str, transforms: &[Transform], row: &dyn Cells) -> Result<Value, RowError> {
    let Some(cell) = row.cell(column) else {
        return Err(RowError {
            column: column.to_owned(),
            reason: RowErrorReason::UnknownColumn,
        });
    };
    run_chain(column, transforms, cell, row)
}

/// A value read as an instant: RFC 3339 text, or the SQL text form read as UTC.
fn instant(value: &Value) -> Option<OffsetDateTime> {
    let widened = match value {
        Value::String(text) => Value::String(sql_timestamp(text).into_owned()),
        other => other.clone(),
    };
    let TraitValue::Timestamp(text) = coerce(TraitType::Timestamp, &widened).ok()? else {
        return None;
    };
    let parsed = chrono::DateTime::parse_from_rfc3339(&text).ok()?;
    let nanos = i128::from(parsed.timestamp()) * 1_000_000_000
        + i128::from(parsed.timestamp_subsec_nanos());
    OffsetDateTime::from_unix_timestamp_nanos(nanos).ok()
}

fn identifier(kind: &str, value: &Value, mapped: &mut Mapped) -> Result<(), RowErrorReason> {
    if value.is_array() || value.is_object() {
        return Err(RowErrorReason::InvalidIdentifier);
    }
    let Some(text) = present(value) else {
        return Ok(());
    };
    let parsed = Identifier::parse(kind, &text).map_err(|_| RowErrorReason::InvalidIdentifier)?;
    let pair = (parsed.kind().as_text(), parsed.value().to_owned());
    if !mapped.identifiers.contains(&pair) {
        mapped.identifiers.push(pair);
    }
    Ok(())
}

fn trait_value(
    key: &str,
    value: Value,
    catalog: &dyn TraitCatalog,
    mapped: &mut Mapped,
) -> Result<(), RowErrorReason> {
    if present(&value).is_none() {
        return Ok(());
    }
    let value = match catalog.trait_state(key) {
        TraitState::Active(kind) => fit(kind, value)?,
        // The profile store drops what the registry does not take.
        TraitState::Missing | TraitState::Archived => value,
    };
    mapped.traits.insert(key.to_owned(), value);
    Ok(())
}

/// Brings `value` to the trait type `kind`: text is cast, anything else is checked.
fn fit(kind: TraitType, value: Value) -> Result<Value, RowErrorReason> {
    let value = match (&value, kind) {
        (Value::String(_), TraitType::String | TraitType::StringList) => value,
        (Value::String(_), _) => cast(kind, value)?,
        _ => value,
    };
    let failure = if kind == TraitType::Timestamp {
        RowErrorReason::BadTimestamp
    } else {
        RowErrorReason::WrongType
    };
    coerce(kind, &value).map(into_json).map_err(|_| failure)
}

fn list_flag(list: Uuid, value: &Value, mapped: &mut Mapped) -> Result<(), RowErrorReason> {
    let joins = match value {
        Value::Bool(flag) => *flag,
        Value::String(text) => parse_flag(text).ok_or(RowErrorReason::WrongType)?,
        Value::Null => false,
        _ => return Err(RowErrorReason::WrongType),
    };
    if joins && !mapped.lists.contains(&list) {
        mapped.lists.push(list);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::model::{ColumnRule, EventRule, PropertyRule, Transform, ValueRule};
    #[test]
    fn an_event_time_reads_sql_timestamp_text_and_rfc_3339_alike() {
        let unix = |value: &str| instant(&json!(value)).map(|at| at.unix_timestamp());
        assert_eq!(unix("2024-01-01 00:01:00+00"), Some(1_704_067_260));
        assert_eq!(unix("2024-01-01 00:01:00"), Some(1_704_067_260));
        assert_eq!(unix("2024-01-01T00:01:00Z"), Some(1_704_067_260));
        assert_eq!(unix("2024-01-01 02:01:00+02:00"), Some(1_704_067_260));
        assert_eq!(unix("2024-01-01"), None);
        assert_eq!(unix("2024-01-01 00:01:00x"), None);
    }
    use super::*;

    fn header(names: &[&str]) -> Header {
        Header::new(names.iter().map(|name| (*name).to_owned()).collect()).unwrap()
    }

    fn cells(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn rule(column: &str, target: Target, transforms: Vec<Transform>) -> ColumnRule {
        ColumnRule {
            column: column.into(),
            target,
            transforms,
        }
    }

    fn email(column: &str) -> ColumnRule {
        rule(
            column,
            Target::Identifier {
                kind: "email".into(),
            },
            vec![Transform::Trim, Transform::Lower],
        )
    }

    fn catalog() -> HashMap<String, TraitState> {
        [
            ("plan", TraitState::Active(TraitType::String)),
            ("age", TraitState::Active(TraitType::Number)),
            ("joined", TraitState::Active(TraitType::Timestamp)),
            ("tags", TraitState::Active(TraitType::StringList)),
            ("born", TraitState::Active(TraitType::Date)),
            ("vip", TraitState::Active(TraitType::Boolean)),
        ]
        .into_iter()
        .map(|(key, state)| (key.to_owned(), state))
        .collect()
    }

    fn run(
        mapping: &Mapping,
        names: &[&str],
        values: &[&str],
    ) -> Result<Mapped, Vec<(String, &'static str)>> {
        let header = header(names);
        let values = cells(values);
        apply(
            mapping,
            &CsvRow {
                header: &header,
                cells: &values,
            },
            &catalog(),
        )
        .map_err(|errors| {
            errors
                .into_iter()
                .map(|error| (error.column, error.reason.as_str()))
                .collect()
        })
    }

    fn mapping(columns: Vec<ColumnRule>) -> Mapping {
        Mapping {
            key_column: None,
            event: None,
            columns,
        }
    }

    #[test]
    fn a_row_becomes_identifiers_traits_and_lists() {
        let list = Uuid::now_v7();
        let mapping = Mapping {
            key_column: Some("id".into()),
            event: None,
            columns: vec![
                email("Email"),
                rule("Plan", Target::Trait { key: "plan".into() }, vec![]),
                rule("Age", Target::Trait { key: "age".into() }, vec![]),
                rule("News", Target::List { list_id: list }, vec![]),
                rule("Note", Target::Ignore, vec![]),
            ],
        };

        let mapped = run(
            &mapping,
            &["id", "Email", "Plan", "Age", "News", "Note"],
            &["u1", " Ada@Example.COM ", "pro", "36", "yes", "skipped"],
        )
        .unwrap();

        assert_eq!(
            mapped.identifiers,
            [("email".to_owned(), "ada@example.com".to_owned())]
        );
        assert_eq!(
            Value::Object(mapped.traits),
            json!({"plan": "pro", "age": 36})
        );
        assert_eq!(mapped.lists, [list]);
        assert_eq!(mapped.key.as_deref(), Some("u1"));
    }

    #[test]
    fn a_row_with_no_identifier_value_is_missing_its_identifier() {
        let mapping = mapping(vec![email("Email")]);

        assert_eq!(
            run(&mapping, &["Email"], &["  "]),
            Err(vec![("Email".to_owned(), "missing_identifier")])
        );
    }

    #[test]
    fn a_bad_identifier_is_invalid_and_never_quoted() {
        let mapping = mapping(vec![email("Email")]);

        let errors = run(&mapping, &["Email"], &["not-an-email"]).unwrap_err();

        assert_eq!(errors, [("Email".to_owned(), "invalid_identifier")]);
    }

    #[test]
    fn a_second_identifier_column_may_fill_in_for_an_empty_first() {
        let mapping = mapping(vec![
            email("Email"),
            rule(
                "Phone",
                Target::Identifier {
                    kind: "phone".into(),
                },
                vec![],
            ),
        ]);

        let mapped = run(&mapping, &["Email", "Phone"], &["", "+45 12 34 56 78"]).unwrap();

        assert_eq!(mapped.identifiers.len(), 1);
        assert_eq!(mapped.identifiers[0].0, "phone");
    }

    #[test]
    fn traits_are_fitted_to_their_registry_type() {
        let mapping = mapping(vec![
            email("Email"),
            rule(
                "Joined",
                Target::Trait {
                    key: "joined".into(),
                },
                vec![Transform::Timestamp {
                    format: "%d/%m/%Y".into(),
                }],
            ),
            rule(
                "Tags",
                Target::Trait { key: "tags".into() },
                vec![Transform::Split {
                    separator: ";".into(),
                }],
            ),
            rule("Born", Target::Trait { key: "born".into() }, vec![]),
            rule("Vip", Target::Trait { key: "vip".into() }, vec![]),
        ]);

        let mapped = run(
            &mapping,
            &["Email", "Joined", "Tags", "Born", "Vip"],
            &["a@example.com", "02/03/2026", "x; y", "1990-05-06", "TRUE"],
        )
        .unwrap();

        assert_eq!(
            Value::Object(mapped.traits),
            json!({
                "joined": "2026-03-02T00:00:00Z",
                "tags": ["x", "y"],
                "born": "1990-05-06",
                "vip": true
            })
        );
    }

    #[test]
    fn a_value_that_does_not_fit_gives_a_code_for_its_column() {
        let mapping = mapping(vec![
            email("Email"),
            rule("Age", Target::Trait { key: "age".into() }, vec![]),
            rule(
                "Joined",
                Target::Trait {
                    key: "joined".into(),
                },
                vec![],
            ),
            rule("Tags", Target::Trait { key: "tags".into() }, vec![]),
            rule(
                "Bad",
                Target::Trait { key: "plan".into() },
                vec![Transform::Timestamp {
                    format: "%d/%m/%Y".into(),
                }],
            ),
        ]);

        let errors = run(
            &mapping,
            &["Email", "Age", "Joined", "Tags", "Bad"],
            &[
                "a@example.com",
                "old",
                "last week",
                "no-split",
                "31/02/2026",
            ],
        )
        .unwrap_err();

        assert_eq!(
            errors,
            [
                ("Age".to_owned(), "wrong_type"),
                ("Joined".to_owned(), "bad_timestamp"),
                ("Tags".to_owned(), "wrong_type"),
                ("Bad".to_owned(), "bad_timestamp"),
            ]
        );
    }

    #[test]
    fn an_empty_trait_cell_leaves_the_trait_out() {
        let mapping = mapping(vec![
            email("Email"),
            rule("Age", Target::Trait { key: "age".into() }, vec![]),
        ]);

        let mapped = run(&mapping, &["Email", "Age"], &["a@example.com", ""]).unwrap();

        assert!(mapped.traits.is_empty());
    }

    #[test]
    fn a_trait_the_registry_lacks_is_passed_on_as_text() {
        let mapping = mapping(vec![
            email("Email"),
            rule(
                "X",
                Target::Trait {
                    key: "fresh".into(),
                },
                vec![],
            ),
        ]);

        let mapped = run(&mapping, &["Email", "X"], &["a@example.com", "v"]).unwrap();

        assert_eq!(Value::Object(mapped.traits), json!({"fresh": "v"}));
    }

    #[test]
    fn a_list_column_joins_on_yes_and_skips_no_and_rejects_nonsense() {
        let list = Uuid::now_v7();
        let mapping = mapping(vec![
            email("Email"),
            rule("L", Target::List { list_id: list }, vec![]),
        ]);
        let join = |cell: &str| run(&mapping, &["Email", "L"], &["a@example.com", cell]);

        assert_eq!(join("x").unwrap().lists, [list]);
        assert_eq!(join("1").unwrap().lists, [list]);
        assert!(join("no").unwrap().lists.is_empty());
        assert!(join("").unwrap().lists.is_empty());
        assert_eq!(join("perhaps"), Err(vec![("L".to_owned(), "wrong_type")]));
    }

    #[test]
    fn a_mapped_column_the_row_lacks_is_an_unknown_column() {
        let mapping = mapping(vec![email("Email"), email("Ghost")]);

        assert_eq!(
            run(&mapping, &["Email"], &["a@example.com"]),
            Err(vec![("Ghost".to_owned(), "unknown_column")])
        );
    }

    #[test]
    fn a_short_row_reads_its_missing_cells_as_empty() {
        let mapping = mapping(vec![
            email("Email"),
            rule("Plan", Target::Trait { key: "plan".into() }, vec![]),
        ]);

        let mapped = run(&mapping, &["Email", "Plan"], &["a@example.com"]).unwrap();

        assert!(mapped.traits.is_empty());
    }

    #[test]
    fn the_key_column_gives_the_key_and_an_empty_cell_gives_none() {
        let mut mapping = mapping(vec![email("Email")]);
        mapping.key_column = Some("id".into());

        assert_eq!(
            run(&mapping, &["Email", "id"], &["a@example.com", " k1 "])
                .unwrap()
                .key
                .as_deref(),
            Some("k1")
        );
        assert_eq!(
            run(&mapping, &["Email", "id"], &["a@example.com", ""])
                .unwrap()
                .key,
            None
        );
        let long = "k".repeat(MAX_KEY_CHARS + 1);
        assert_eq!(
            run(&mapping, &["Email", "id"], &["a@example.com", &long]),
            Err(vec![("id".to_owned(), "invalid_key")])
        );
    }

    #[test]
    fn every_error_of_a_row_is_reported_and_none_holds_a_value() {
        let mapping = mapping(vec![
            email("Email"),
            rule("Age", Target::Trait { key: "age".into() }, vec![]),
        ]);

        let errors = run(
            &mapping,
            &["Email", "Age"],
            &["secret-not-email", "secret-age"],
        )
        .unwrap_err();

        assert_eq!(errors.len(), 2);
        assert!(
            errors.iter().all(|(column, reason)| {
                !column.contains("secret") && !reason.contains("secret")
            })
        );
    }

    fn order_event(timestamp: Option<ValueRule>, properties: Vec<PropertyRule>) -> EventRule {
        EventRule {
            name: "order_placed".into(),
            timestamp,
            properties,
        }
    }

    fn property(name: &str, column: &str, transforms: Vec<Transform>) -> PropertyRule {
        PropertyRule {
            property: name.into(),
            column: column.into(),
            transforms,
        }
    }

    fn with_event(event: EventRule) -> Mapping {
        Mapping {
            event: Some(event),
            ..mapping(vec![email("Email")])
        }
    }

    #[test]
    fn a_row_yields_one_event_with_its_timestamp_and_non_empty_properties() {
        let mapping = with_event(order_event(
            Some(ValueRule {
                column: "When".into(),
                transforms: vec![Transform::Timestamp {
                    format: "%d/%m/%Y %H:%M".into(),
                }],
            }),
            vec![
                property(
                    "total",
                    "Total",
                    vec![Transform::Cast {
                        to: TraitType::Number,
                    }],
                ),
                property("note", "Note", vec![]),
                property(
                    "tags",
                    "Tags",
                    vec![Transform::Split {
                        separator: ";".into(),
                    }],
                ),
            ],
        ));

        let mapped = run(
            &mapping,
            &["Email", "When", "Total", "Note", "Tags"],
            &["a@example.com", "02/03/2026 10:30", "12.5", "  ", "x; y"],
        )
        .unwrap();

        let event = mapped.event.unwrap();
        assert_eq!(event.name, "order_placed");
        assert_eq!(
            event.occurred_at,
            Some(time::macros::datetime!(2026-03-02 10:30 UTC))
        );
        assert_eq!(
            Value::Object(event.properties),
            json!({"total": 12.5, "tags": ["x", "y"]})
        );
    }

    #[test]
    fn an_event_without_a_timestamp_or_with_an_empty_one_has_no_occurred_at() {
        let without = with_event(order_event(None, vec![]));
        let empty = with_event(order_event(
            Some(ValueRule {
                column: "When".into(),
                transforms: vec![],
            }),
            vec![],
        ));

        let a = run(&without, &["Email"], &["a@example.com"]).unwrap();
        let b = run(&empty, &["Email", "When"], &["a@example.com", ""]).unwrap();

        assert_eq!(a.event.unwrap().occurred_at, None);
        assert_eq!(b.event.unwrap().occurred_at, None);
        assert!(
            run(
                &mapping(vec![email("Email")]),
                &["Email"],
                &["a@example.com"]
            )
            .unwrap()
            .event
            .is_none()
        );
    }

    #[test]
    fn an_event_that_cannot_be_read_gives_codes_for_its_columns() {
        let mapping = with_event(order_event(
            Some(ValueRule {
                column: "When".into(),
                transforms: vec![],
            }),
            vec![
                property(
                    "total",
                    "Total",
                    vec![Transform::Cast {
                        to: TraitType::Number,
                    }],
                ),
                property("ghost", "Ghost", vec![]),
            ],
        ));

        let errors = run(
            &mapping,
            &["Email", "When", "Total"],
            &["a@example.com", "secret-when", "secret-total"],
        )
        .unwrap_err();

        assert_eq!(
            errors,
            [
                ("When".to_owned(), "bad_timestamp"),
                ("Total".to_owned(), "wrong_type"),
                ("Ghost".to_owned(), "unknown_column"),
            ]
        );
        assert!(!format!("{errors:?}").contains("secret"));
    }

    #[test]
    fn a_row_error_never_holds_a_cell_value_for_any_target_or_the_event() {
        let list = Uuid::now_v7();
        let mut mapping = mapping(vec![
            email("Email"),
            rule("Age", Target::Trait { key: "age".into() }, vec![]),
            rule("L", Target::List { list_id: list }, vec![]),
        ]);
        mapping.event = Some(order_event(
            Some(ValueRule {
                column: "When".into(),
                transforms: vec![],
            }),
            vec![],
        ));
        let header = header(&["Email", "Age", "L", "When"]);
        let cells = cells(&[
            "secret-email",
            "secret-age",
            "secret-flag",
            "secret-instant",
        ]);

        let errors = apply(
            &mapping,
            &CsvRow {
                header: &header,
                cells: &cells,
            },
            &catalog(),
        )
        .unwrap_err();

        assert_eq!(errors.len(), 4);
        assert!(!format!("{errors:?}").contains("secret"));
    }

    #[test]
    fn map_json_reads_nested_paths_and_equals_apply_over_the_record() {
        let list = Uuid::now_v7();
        let mut mapping = Mapping {
            key_column: Some("ids.0".into()),
            ..mapping(vec![
                email("profile.email"),
                rule(
                    "profile.age",
                    Target::Trait { key: "age".into() },
                    vec![Transform::Cast {
                        to: TraitType::Number,
                    }],
                ),
                rule("flags.news", Target::List { list_id: list }, vec![]),
            ])
        };
        mapping.event = Some(order_event(
            None,
            vec![property("sku", "orders.0.sku", vec![])],
        ));
        let data = json!({
            "ids": ["k-1"],
            "profile": {"email": " Ada@Example.com ", "age": 36},
            "flags": {"news": true},
            "orders": [{"sku": "A-1"}]
        });
        let declared = ["ids", "profile", "flags", "orders"];

        let mapped = map_json(&mapping, &data, declared, &catalog()).unwrap();

        assert_eq!(
            mapped.identifiers,
            [("email".to_owned(), "ada@example.com".to_owned())]
        );
        assert_eq!(mapped.key.as_deref(), Some("k-1"));
        assert_eq!(mapped.traits.get("age"), Some(&json!(36)));
        assert_eq!(mapped.lists, [list]);
        assert_eq!(
            mapped.event.as_ref().unwrap().properties.get("sku"),
            Some(&json!("A-1"))
        );
        let record = JsonRecord::from_value_for_mapping(&data, &mapping, declared);
        assert_eq!(apply(&mapping, &record, &catalog()), Ok(mapped));
    }

    #[test]
    fn map_json_reports_an_undeclared_path_as_unknown_and_a_declared_one_as_empty() {
        let mapping = Mapping {
            event: Some(order_event(
                None,
                vec![property("p", "orders.0.sku", vec![])],
            )),
            ..mapping(vec![email("profile.email")])
        };
        let data = json!({"profile": {"email": "a@example.com"}});

        let undeclared = map_json(&mapping, &data, ["profile"], &catalog()).unwrap_err();
        let declared = map_json(&mapping, &data, ["profile", "orders"], &catalog()).unwrap();

        assert_eq!(undeclared[0].column, "orders.0.sku");
        assert_eq!(undeclared[0].reason, RowErrorReason::UnknownColumn);
        assert!(declared.event.unwrap().properties.is_empty());
    }

    #[test]
    fn a_mapping_saved_before_events_serializes_back_to_the_same_json_and_applies_the_same() {
        let old = json!({
            "key_column": "id",
            "columns": [
                {"column": "Email", "target": {"type": "identifier", "kind": "email"},
                 "transforms": [{"op": "trim"}, {"op": "lower"}]},
                {"column": "Plan", "target": {"type": "trait", "key": "plan"}, "transforms": []}
            ]
        });

        let mapping: Mapping = serde_json::from_value(old.clone()).unwrap();

        assert_eq!(serde_json::to_value(&mapping).unwrap(), old);
        assert!(!mapping.has_event());
        let mapped = run(
            &mapping,
            &["id", "Email", "Plan"],
            &["u1", " A@Example.com ", "pro"],
        )
        .unwrap();
        assert_eq!(mapped.identifiers[0].1, "a@example.com");
        assert_eq!(mapped.event, None);
        assert_eq!(mapped.key.as_deref(), Some("u1"));
    }
    #[test]
    fn a_header_must_have_unique_non_blank_names() {
        assert_eq!(
            Header::new(vec!["a".into(), "a".into()]),
            Err(HeaderError::Duplicate)
        );
        assert_eq!(
            Header::new(vec!["a".into(), " ".into()]),
            Err(HeaderError::Blank)
        );
        assert!(Header::new(vec!["a".into(), "A".into()]).is_ok());
    }
}
