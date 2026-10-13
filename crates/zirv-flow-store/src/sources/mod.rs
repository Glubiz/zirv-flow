//! Sources and their streams (architecture sections 5.3 and 7.2): the configured external
//! systems and the per-stream sync settings. A source's credential is a sealed secret,
//! referenced by id and never stored here.

mod crud;
mod schedule;
mod streams;

pub use crud::{
    NewSource, Source, SourceUpdate, create_source, delete_source, find_source,
    find_source_for_update, list_sources, update_source,
};
pub use schedule::sync_schedule;
pub use streams::{
    DiscoveredStream, Stream, StreamSettings, configure_stream, find_stream, find_stream_by_id,
    list_streams, set_stream_mapping, upsert_discovered_streams,
};
