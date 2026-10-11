//! Applying a mapping to one row: a profile record's parts, or the codes of what is wrong.

use std::collections::HashMap;

use serde_json::{Map, Value};
use uuid::Uuid;

use super::model::{ColumnRule, Mapping, Target};
use super::transform::{apply_transform, cast, into_json, parse_flag};
use super::validate::{TraitCatalog, TraitState};
use crate::people::{Identifier, TraitType, coerce};

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
        let value = match run_chain(rule, cell, row) {
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

/// A cell after the transforms of its rule.
fn run_chain(rule: &ColumnRule, cell: &str, row: &dyn Cells) -> Result<Value, RowError> {
    let mut value = Value::String(cell.to_owned());
    for transform in &rule.transforms {
        value = apply_transform(transform, value, row, &rule.column)
            .map_err(|(column, reason)| RowError { column, reason })?;
    }
    Ok(value)
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

    use super::super::model::{ColumnRule, Transform};
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
            columns,
        }
    }

    #[test]
    fn a_row_becomes_identifiers_traits_and_lists() {
        let list = Uuid::now_v7();
        let mapping = Mapping {
            key_column: Some("id".into()),
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
