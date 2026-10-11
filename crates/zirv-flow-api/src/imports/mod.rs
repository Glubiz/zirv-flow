//! CSV imports (architecture sections 7.1 and 7.2): upload a file, map its columns, dry-run
//! the mapping, run it with checkpoints, and read the history and the rejected rows.
//!
//! No log line, metric label, audit entry, problem body or reject listing holds a cell of
//! the file. The one place the cells come back is the rejects CSV, and only for the owner and
//! admins; the same goes for the sample rows an upload shows.

pub(crate) mod crud;
pub(crate) mod rejects;
pub(crate) mod upload;

use std::collections::BTreeMap;

use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::TraceId;
use zirv_flow_core::mapping::Mapping;
use zirv_flow_store::{Import, TransitionError};

use crate::problem::ApiError;

/// What an upload may be.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImportLimits {
    /// The largest file, in bytes.
    pub max_file_bytes: u64,
}

impl Default for ImportLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 512 * 1024 * 1024,
        }
    }
}

/// The sample rows an upload shows.
pub(crate) const SAMPLE_ROWS: usize = 20;

/// An import: its file, mapping, state and counters.
#[derive(Serialize, ToSchema)]
pub(crate) struct CsvImportBody {
    id: Uuid,
    #[schema(example = "Customers May")]
    name: String,
    /// The source the import's records are written under.
    #[schema(example = "csv")]
    source: String,
    /// The stream the import's records are written under; keep it stable between runs of the
    /// same data.
    #[schema(example = "customers")]
    stream: String,
    file_name: String,
    /// The SHA-256 of the file, in lowercase hex.
    file_sha256: String,
    size_bytes: i64,
    /// `,`, `;` or a tab.
    delimiter: String,
    /// The column names, in file order.
    header: Vec<String>,
    mapping: Option<MappingBody>,
    /// `uploaded`, `mapped`, `dry_running`, `dry_run_done`, `running`, `succeeded`, `failed` or
    /// `cancelled`.
    status: String,
    /// Data rows in the file, the header not counted.
    rows_total: i64,
    /// Rows the current pass has finished, rejected ones included.
    rows_done: i64,
    /// Rows applied (written); in a dry run, rows that would be.
    rows_applied: i64,
    /// Rows that repeated what is stored already, so nothing was written.
    rows_unchanged: i64,
    rows_rejected: i64,
    /// The rejected share above which a run stops, once more than 1,000 rows are done.
    reject_threshold: f64,
    /// Why a failed import failed, such as `reject_threshold`; never a value.
    error_code: Option<String>,
    created_by: Option<Uuid>,
    /// The trace of the upload: 32 lowercase hex characters.
    trace_id: String,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    updated_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    started_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    finished_at: Option<OffsetDateTime>,
}

impl From<&Import> for CsvImportBody {
    fn from(import: &Import) -> Self {
        Self {
            id: import.id,
            name: import.name.clone(),
            source: import.source.clone(),
            stream: import.stream.clone(),
            file_name: import.file_name.clone(),
            file_sha256: hex(&import.file_sha256),
            size_bytes: import.size_bytes,
            delimiter: import.delimiter.to_string(),
            header: import.header.clone(),
            mapping: import.mapping.as_ref().and_then(MappingBody::from_core),
            status: import.status.as_str().to_owned(),
            rows_total: import.rows_total,
            rows_done: import.rows_done,
            rows_applied: import.rows_applied,
            rows_unchanged: import.rows_unchanged,
            rows_rejected: import.rows_rejected,
            reject_threshold: import.reject_threshold,
            error_code: import.error_code.clone(),
            created_by: import.created_by,
            trace_id: TraceId::from_uuid(import.trace_id).to_string(),
            created_at: import.created_at,
            updated_at: import.updated_at,
            started_at: import.started_at,
            finished_at: import.finished_at,
        }
    }
}

/// An import with the first rows of its file.
#[derive(Serialize, ToSchema)]
pub(crate) struct CsvImportDetailBody {
    #[serde(flatten)]
    import: CsvImportBody,
    /// The first 20 data rows, one list of cells per row, in header order. Empty unless asked
    /// for (`samples=true`) or just uploaded, and empty once the file was deleted.
    samples: Vec<Vec<Option<String>>>,
    /// True when the caller may not see values: every cell of `samples` is null.
    samples_masked: bool,
}

impl CsvImportDetailBody {
    pub(crate) fn new(import: &Import, samples: Vec<Vec<String>>, may_see_values: bool) -> Self {
        let samples = samples
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|cell| may_see_values.then_some(cell))
                    .collect()
            })
            .collect();
        Self {
            import: CsvImportBody::from(import),
            samples,
            samples_masked: !may_see_values,
        }
    }
}

/// How the columns of a file become identifiers, traits and lists.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct MappingBody {
    /// The column whose value is the record key. Without it the key is the file's hash and the
    /// row number, so only the same file can be run again without changes.
    key_column: Option<String>,
    columns: Vec<ColumnRuleBody>,
}

impl MappingBody {
    fn from_core(mapping: &Mapping) -> Option<Self> {
        serde_json::from_value(serde_json::to_value(mapping).ok()?).ok()
    }

    pub(crate) fn to_core(&self) -> Result<Mapping, ApiError> {
        let value = serde_json::to_value(self).map_err(|_| ApiError::internal())?;
        serde_json::from_value(value).map_err(|_| ApiError::internal())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ColumnRuleBody {
    /// A column name of the header.
    column: String,
    target: TargetBody,
    /// Applied in order to the cell before it reaches the target; at most 10.
    #[serde(default)]
    transforms: Vec<TransformBody>,
}

/// Where a column's value goes.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum TargetBody {
    /// An identifier: `email`, `phone`, `anonymous_id` or `external:<namespace>`.
    Identifier { kind: String },
    /// A trait by registry key; it must exist and not be archived.
    Trait { key: String },
    /// A truthy cell (`true`, `1`, `yes`, `x`, and so on) adds the person to the list.
    List { list_id: Uuid },
    /// The column is not used.
    Ignore,
}

/// The type a `cast` converts to.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TraitTypeBody {
    String,
    Number,
    Boolean,
    Timestamp,
    Date,
    StringList,
}

/// One step between a cell and its target.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum TransformBody {
    Trim,
    Lower,
    Upper,
    /// Converts the text to a type.
    Cast {
        to: TraitTypeBody,
    },
    /// Parses a date or time into a UTC timestamp. `format` is a strftime pattern such as
    /// `%d/%m/%Y %H:%M`, or `unix` or `unix_ms`.
    Timestamp {
        format: String,
    },
    /// Joins the value with other columns' values, skipping empty ones.
    Concat {
        columns: Vec<String>,
        separator: String,
    },
    /// Ignores the cell and uses this text.
    Constant {
        value: String,
    },
    /// Replaces a value that is a key of the map; others stay as they are.
    ValueMap {
        map: BTreeMap<String, String>,
    },
    /// Splits the text into a list of trimmed, non-empty strings.
    Split {
        separator: String,
    },
}

/// The lowercase hex of `bytes`.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;

    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// The answer to a state change the import's state does not allow.
pub(crate) fn transition_error(error: TransitionError) -> ApiError {
    match error {
        TransitionError::NotFound => ApiError::not_found(),
        TransitionError::WrongState(status) => {
            ApiError::new(StatusCode::CONFLICT, format!("the import is {status}"))
        }
        TransitionError::Store(error) => error.into(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_default_limit_is_512_mib() {
        assert_eq!(ImportLimits::default().max_file_bytes, 536_870_912);
    }

    #[test]
    fn a_mapping_body_round_trips_through_the_core_mapping() {
        let value = json!({
            "key_column": "id",
            "columns": [
                {"column": "Email", "target": {"type": "identifier", "kind": "email"},
                 "transforms": [{"op": "trim"}, {"op": "lower"}]},
                {"column": "Born", "target": {"type": "trait", "key": "born"},
                 "transforms": [{"op": "cast", "to": "date"},
                                {"op": "value_map", "map": {"a": "b"}}]},
                {"column": "Skip", "target": {"type": "ignore"}, "transforms": []}
            ]
        });
        let body: MappingBody = serde_json::from_value(value.clone()).unwrap();

        let core = body.to_core().unwrap();
        let back = MappingBody::from_core(&core).unwrap();

        assert_eq!(serde_json::to_value(&back).unwrap(), value);
        assert_eq!(core.columns.len(), 3);
    }

    #[test]
    fn a_mapping_with_an_unknown_field_or_operation_is_refused() {
        for bad in [
            json!({"columns": [], "x": 1}),
            json!({"columns": [{"column": "a", "target": {"type": "nope"}}]}),
            json!({"columns": [{"column": "a", "target": {"type": "ignore"},
                                "transforms": [{"op": "eval", "code": "1"}]}]}),
        ] {
            assert!(serde_json::from_value::<MappingBody>(bad).is_err());
        }
    }

    #[test]
    fn samples_are_nulled_and_flagged_for_a_caller_who_may_not_see_values() {
        let rows = vec![vec!["a@example.com".to_owned(), "x".to_owned()]];
        let import = sample_import();

        let masked = CsvImportDetailBody::new(&import, rows.clone(), false);
        let shown = CsvImportDetailBody::new(&import, rows, true);

        assert_eq!(masked.samples, [[None, None]]);
        assert!(masked.samples_masked);
        assert_eq!(
            shown.samples,
            [[Some("a@example.com".to_owned()), Some("x".to_owned())]]
        );
        assert!(!shown.samples_masked);
    }

    fn sample_import() -> Import {
        Import {
            id: Uuid::nil(),
            workspace_id: Uuid::nil(),
            name: "n".into(),
            source: "csv".into(),
            stream: "s".into(),
            file_name: "f.csv".into(),
            file_sha256: [0xab; 32],
            size_bytes: 1,
            delimiter: ',',
            header: vec!["a".into(), "b".into()],
            mapping: None,
            status: zirv_flow_store::ImportStatus::Uploaded,
            rows_total: 1,
            rows_done: 0,
            rows_applied: 0,
            rows_unchanged: 0,
            rows_rejected: 0,
            reject_threshold: 0.05,
            error_code: None,
            created_by: None,
            trace_id: Uuid::nil(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            started_at: None,
            finished_at: None,
        }
    }

    #[test]
    fn an_import_is_shown_with_its_hash_in_hex_and_no_cells() {
        let body = serde_json::to_value(CsvImportBody::from(&sample_import())).unwrap();

        assert_eq!(body["file_sha256"], "ab".repeat(32));
        assert_eq!(body["status"], "uploaded");
        assert_eq!(body["trace_id"].as_str().unwrap().len(), 32);
        assert_eq!(body["created_at"], "1970-01-01T00:00:00Z");
        assert_eq!(body["delimiter"], ",");
    }

    #[test]
    fn a_wrong_state_is_a_409_and_a_missing_import_a_404() {
        let wrong = transition_error(TransitionError::WrongState(
            zirv_flow_store::ImportStatus::Running,
        ));

        assert_eq!(wrong.status(), StatusCode::CONFLICT);
        assert_eq!(wrong.detail(), "the import is running");
        assert_eq!(
            transition_error(TransitionError::NotFound).status(),
            StatusCode::NOT_FOUND
        );
    }
}
