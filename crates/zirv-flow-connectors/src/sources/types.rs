//! The values a source yields and the descriptors it reports.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The type of one field of a stream.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    String,
    Number,
    Boolean,
    Timestamp,
    Json,
    Unknown,
}

/// One field of a discovered stream.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FieldDescriptor {
    pub name: String,
    pub field_type: FieldType,
}

/// How a stream is read.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    /// Read everything on every run.
    FullRefresh,
    /// Read what changed since the checkpoint, ordered by a cursor field.
    Incremental,
}

impl SyncMode {
    /// The value stored in `streams.sync_mode`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FullRefresh => "full_refresh",
            Self::Incremental => "incremental",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "full_refresh" => Some(Self::FullRefresh),
            "incremental" => Some(Self::Incremental),
            _ => None,
        }
    }
}

/// A stream a source offers, as discovery reports it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StreamDescriptor {
    pub name: String,
    pub fields: Vec<FieldDescriptor>,
    /// The modes the stream supports; never empty.
    pub sync_modes: Vec<SyncMode>,
    pub default_cursor_field: Option<String>,
    pub default_primary_key: Vec<String>,
}

/// How a stream is configured for a read: what the user chose from the descriptor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamConfig {
    pub name: String,
    pub sync_mode: SyncMode,
    pub cursor_field: Option<String>,
    pub primary_key: Vec<String>,
}

/// One record of a stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Record {
    /// The record's identity within its stream, from the primary key.
    pub key: String,
    pub data: Value,
    /// True when the upstream reports the record as deleted.
    pub deleted: bool,
}

/// Where to resume a stream. Opaque to everything but the source that wrote it (architecture
/// 7.2): the engine stores and returns it and never looks inside.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Checkpoint(pub Value);

/// Records read together with the checkpoint that resumes after them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Batch {
    pub records: Vec<Record>,
    pub checkpoint: Checkpoint,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_sync_mode_round_trips_through_its_stored_text() {
        for mode in [SyncMode::FullRefresh, SyncMode::Incremental] {
            assert_eq!(SyncMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(SyncMode::parse("other"), None);
    }

    #[test]
    fn a_descriptor_round_trips_as_json() {
        let descriptor = StreamDescriptor {
            name: "contacts".into(),
            fields: vec![FieldDescriptor {
                name: "id".into(),
                field_type: FieldType::Number,
            }],
            sync_modes: vec![SyncMode::FullRefresh, SyncMode::Incremental],
            default_cursor_field: Some("updated_at".into()),
            default_primary_key: vec!["id".into()],
        };

        let value = serde_json::to_value(&descriptor).unwrap();

        assert_eq!(value["fields"][0]["field_type"], "number");
        assert_eq!(value["sync_modes"][1], "incremental");
        assert_eq!(
            serde_json::from_value::<StreamDescriptor>(value).unwrap(),
            descriptor
        );
    }

    #[test]
    fn a_checkpoint_serializes_as_its_bare_value() {
        let checkpoint = Checkpoint(json!({"offset": 3}));

        assert_eq!(
            serde_json::to_string(&checkpoint).unwrap(),
            r#"{"offset":3}"#
        );
    }
}
