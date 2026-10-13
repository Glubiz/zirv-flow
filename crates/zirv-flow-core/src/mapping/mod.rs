//! Mapping a tabular row to a profile record (architecture section 7.2): which column is an
//! identifier, a trait or a list, and the bounded set of transforms between the cell and the
//! value. Pure, so the CSV import and the M3 sync runs share one mapping core.
//!
//! No scripting in v1 (OQ-5). Errors are codes only: a row error names the column and the
//! rule it broke, never the cell.

mod apply;
mod json_record;
mod model;
mod sample;
mod transform;
mod validate;

pub use apply::{
    Cells, CsvRow, Deletion, Header, HeaderError, MAX_EVENT_PROPERTIES, MAX_KEY_CHARS, Mapped,
    MappedEvent, RowError, RowErrorReason, apply, deletion, is_deleted, map_json,
};
pub use json_record::{JsonRecord, walk_path};
pub use model::{
    ColumnRule, DeleteRule, DeleteWhen, EventRule, Mapping, PropertyRule, Target, Transform,
    ValueRule,
};
pub use sample::{RejectSample, SAMPLE_STRING_CHARS, reject_sample};
pub use validate::{MappingError, TraitCatalog, TraitState, validate, validate_paths};
