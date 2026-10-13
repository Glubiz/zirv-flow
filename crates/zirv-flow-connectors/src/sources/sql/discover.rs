//! What both database sources do the same way when they discover and open streams.

use super::settings::{QUERY_PREFIX, Settings, config_error};
use crate::sources::{FieldDescriptor, FieldType, SourceError, StreamDescriptor, SyncMode};

/// More tables than this in the schema or database is refused: grant the role fewer.
pub(crate) const MAX_TABLES: usize = 1000;

/// A stream with no fields yet, offering both sync modes.
pub(crate) fn descriptor(name: String) -> StreamDescriptor {
    StreamDescriptor {
        name,
        fields: Vec::new(),
        sync_modes: vec![SyncMode::FullRefresh, SyncMode::Incremental],
        default_cursor_field: None,
        default_primary_key: Vec::new(),
        suggested_mapping: None,
    }
}

/// `updated_at` when the stream has a timestamp column of that name.
pub(crate) fn default_cursor(fields: &[FieldDescriptor]) -> Option<String> {
    fields
        .iter()
        .find(|field| field.name == "updated_at" && field.field_type == FieldType::Timestamp)
        .map(|field| field.name.clone())
}

/// The table streams from the catalog: `columns` are `(table, field)` rows ordered by table, and
/// `keys` the `(table, column)` primary-key rows in key order. `too_many` is the refusal for more
/// than [`MAX_TABLES`] tables.
pub(crate) fn table_streams(
    columns: impl IntoIterator<Item = (String, FieldDescriptor)>,
    keys: impl IntoIterator<Item = (String, String)>,
    too_many: impl FnOnce() -> SourceError,
) -> Result<Vec<StreamDescriptor>, SourceError> {
    let mut streams: Vec<StreamDescriptor> = Vec::new();
    for (table, field) in columns {
        if streams.last().is_none_or(|stream| stream.name != table) {
            if streams.len() == MAX_TABLES {
                return Err(too_many());
            }
            streams.push(descriptor(table));
        }
        streams
            .last_mut()
            .expect("a stream was just pushed")
            .fields
            .push(field);
    }
    for (table, column) in keys {
        if let Some(stream) = streams.iter_mut().find(|stream| stream.name == table) {
            stream.default_primary_key.push(column);
        }
    }
    for stream in &mut streams {
        stream.default_cursor_field = default_cursor(&stream.fields);
    }
    Ok(streams)
}

/// The stream of a configured query, from the columns the server reported. A column name twice
/// would make a record ambiguous.
pub(crate) fn query_stream(
    name: &str,
    fields: impl IntoIterator<Item = FieldDescriptor>,
) -> Result<StreamDescriptor, SourceError> {
    let mut stream = descriptor(format!("{QUERY_PREFIX}{name}"));
    for field in fields {
        if stream.fields.iter().any(|other| other.name == field.name) {
            return Err(config_error("queries", "returns a column name twice"));
        }
        stream.fields.push(field);
    }
    Ok(stream)
}

/// What a stream reads from: a configured query (`query:<name>`), parenthesised, or the table
/// `table` renders to its qualified, quoted name.
pub(crate) fn from_clause(
    settings: &Settings,
    stream: &str,
    table: impl FnOnce(&str) -> String,
) -> Result<String, SourceError> {
    if stream.starts_with(QUERY_PREFIX) {
        let query = settings
            .query_for(stream)
            .ok_or_else(|| config_error("queries", "has no query for this stream"))?;
        // Newlines keep a trailing line comment from swallowing the closing parenthesis.
        return Ok(format!("(\n{}\n)", query.sql));
    }
    Ok(table(stream))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::sources::sql::settings::Flavor;

    fn field(name: &str, field_type: FieldType) -> FieldDescriptor {
        FieldDescriptor {
            name: name.into(),
            field_type,
        }
    }

    #[test]
    fn updated_at_is_the_default_cursor_only_when_it_is_a_timestamp() {
        assert_eq!(
            default_cursor(&[field("updated_at", FieldType::Timestamp)]),
            Some("updated_at".into())
        );
        assert_eq!(
            default_cursor(&[field("updated_at", FieldType::String)]),
            None
        );
        assert_eq!(
            default_cursor(&[field("created_at", FieldType::Timestamp)]),
            None
        );
    }

    #[test]
    fn a_query_stream_is_wrapped_on_its_own_lines_and_a_table_is_rendered_by_the_caller() {
        let settings = Settings::parse(
            &json!({"host": "h", "database": "d",
                "queries": [{"name": "vip", "sql": "SELECT 1 -- note"}]}),
            Flavor::Postgres,
        )
        .unwrap();

        assert_eq!(
            from_clause(&settings, "query:vip", |_| unreachable!()).unwrap(),
            "(\nSELECT 1 -- note\n)"
        );
        assert_eq!(
            from_clause(&settings, "contacts", |t| format!("[{t}]")).unwrap(),
            "[contacts]"
        );
        assert_eq!(
            from_clause(&settings, "query:other", |_| unreachable!()).unwrap_err(),
            config_error("queries", "has no query for this stream")
        );
    }

    #[test]
    fn table_streams_group_columns_by_table_and_take_keys_and_the_default_cursor() {
        let columns = [
            ("a", field("id", FieldType::Number)),
            ("a", field("updated_at", FieldType::Timestamp)),
            ("b", field("id", FieldType::Number)),
        ]
        .map(|(table, field)| (table.to_owned(), field));
        let keys = [("a", "id"), ("zz", "id"), ("b", "id")]
            .map(|(table, column)| (table.to_owned(), column.to_owned()));

        let streams = table_streams(columns, keys, || unreachable!()).unwrap();

        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0].fields.len(), 2);
        assert_eq!(streams[0].default_primary_key, ["id"]);
        assert_eq!(
            streams[0].default_cursor_field.as_deref(),
            Some("updated_at")
        );
        assert_eq!(streams[1].default_cursor_field, None);
    }

    #[test]
    fn more_than_the_most_tables_is_refused_with_the_callers_error() {
        let columns = (0..=MAX_TABLES).map(|n| (format!("t{n}"), field("id", FieldType::Number)));

        let error = table_streams(columns, [], || config_error("schema", "too many")).unwrap_err();

        assert_eq!(error, config_error("schema", "too many"));
    }

    #[test]
    fn a_query_stream_is_prefixed_and_a_repeated_column_name_is_refused() {
        let stream = query_stream("vip", [field("id", FieldType::Number)]).unwrap();
        let twice = query_stream(
            "vip",
            [
                field("id", FieldType::Number),
                field("id", FieldType::String),
            ],
        );

        assert_eq!(stream.name, "query:vip");
        assert_eq!(
            twice.unwrap_err(),
            config_error("queries", "returns a column name twice")
        );
    }

    #[test]
    fn a_new_stream_offers_both_modes_and_no_defaults() {
        let stream = descriptor("t".into());

        assert_eq!(
            stream.sync_modes,
            [SyncMode::FullRefresh, SyncMode::Incremental]
        );
        assert!(stream.default_primary_key.is_empty() && stream.default_cursor_field.is_none());
    }
}
