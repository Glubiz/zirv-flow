//! Stream offsets (architecture 7.6): where a consumer resumes, per partition of the topic a
//! stream reads, and how far behind it is. The offsets are written inside the transaction that
//! applies the batch they cover ([`crate::commit_sync_batch`]); this module reads them and
//! reports lag. A number is all it holds: no person, record key or value.

mod offsets;

pub use offsets::{
    LagReport, OffsetsStored, PartitionLag, PartitionOffset, commit_stream_offsets, load_offsets,
    report_stream_lag, stream_lag,
};
