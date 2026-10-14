//! Segments with versions (architecture sections 6, 7.5 and 8): the catalog a definition is
//! checked against, saving versions under the cycle check, and the preview queries with data age.
//! This crate never sees the compiler: the API validates and compiles, and passes in the checked
//! content and the SQL.

mod catalog;
mod crud;
mod cycle;
mod data_age;
mod preview;

pub use catalog::{CatalogSnapshot, segment_catalog};
pub use crud::{
    CADENCE_MINUTES, DEFAULT_CADENCE_MINUTES, NewSegment, NewSegmentVersion, Segment, SegmentError,
    SegmentPatch, SegmentVersion, archive_segment, create_segment, find_segment,
    find_segment_version, list_segment_versions, list_segments, lock_segments, update_segment,
};
pub use cycle::SegmentRef;
pub use data_age::{DataAge, FeedingSource, segment_data_age};
pub use preview::{
    SampleRow, SqlParam, SqlQuery, begin_segment_snapshot, run_segment_preview, snapshot_now,
};
