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
    }
}

/// `updated_at` when the stream has a timestamp column of that name.
pub(crate) fn default_cursor(fields: &[FieldDescriptor]) -> Option<String> {
    fields
        .iter()
        .find(|field| field.name == "updated_at" && field.field_type == FieldType::Timestamp)
        .map(|field| field.name.clone())
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
    fn a_new_stream_offers_both_modes_and_no_defaults() {
        let stream = descriptor("t".into());

        assert_eq!(
            stream.sync_modes,
            [SyncMode::FullRefresh, SyncMode::Incremental]
        );
        assert!(stream.default_primary_key.is_empty() && stream.default_cursor_field.is_none());
    }
}
