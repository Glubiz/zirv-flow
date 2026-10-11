//! Mapping a tabular row to a profile record (architecture section 7.2): which column is an
//! identifier, a trait or a list, and the bounded set of transforms between the cell and the
//! value. Pure, so the CSV import and the M3 sync runs share one mapping core.
//!
//! No scripting in v1 (OQ-5). Errors are codes only: a row error names the column and the
//! rule it broke, never the cell.

mod apply;
mod model;
mod transform;
mod validate;

pub use apply::{
    Cells, CsvRow, Header, HeaderError, MAX_KEY_CHARS, Mapped, RowError, RowErrorReason, apply,
};
pub use model::{ColumnRule, Mapping, Target, Transform};
pub use validate::{MappingError, TraitCatalog, TraitState, validate};
