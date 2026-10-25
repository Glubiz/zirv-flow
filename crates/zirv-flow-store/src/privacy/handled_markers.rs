//! The handled markers (architecture 16.3, "As built (handled markers, #283)").
//!
//! An erasure calls every destination that handled the person, and finds them in the person's
//! messages, forwarding outbox rows, `sync.*` events and `delivery.sync` jobs. Retention removes
//! some of those rows. Each purge that does writes a marker, `erasure_handled_markers`, for the
//! (person, destination) pairs of the rows it removes, in the same statement. Nothing on a hot path
//! writes one. `handled` (in `erasure_calls`) reads the markers with the live rows, and
//! `delete_person` deletes them in the erasure transaction.
//!
//! The marker insert joins `people`, so it writes nothing for a person who is already gone, and
//! the table's person key orders the rest: a purge that raced an erasure to the person row fails
//! the whole statement (the rows it deleted come back) and runs again on its next pass.

use zirv_flow_core::events::{SYNC_FAILED_EVENT, SYNC_SUCCEEDED_EVENT};

/// The statement that writes the markers of the rows a purge removed. It reads the CTE `gone`,
/// whose columns are `workspace_id`, `person_id`, `destination` (the destination id as text, or
/// NULL for a row that names none) and `handled_at`. A destination that no longer exists is skipped, and
/// the text comparison cannot fail on a malformed value.
const WRITE_MARKERS: &str = "INSERT INTO erasure_handled_markers \
        (workspace_id, person_id, destination_id, first_handled_at) \
    SELECT g.workspace_id, g.person_id, d.id, min(g.handled_at) FROM gone g \
    JOIN people p ON p.workspace_id = g.workspace_id AND p.id = g.person_id \
    JOIN destinations d ON d.workspace_id = g.workspace_id AND d.id::text = g.destination \
    GROUP BY g.workspace_id, g.person_id, d.id \
    ON CONFLICT DO NOTHING";

/// `gone` is a `DELETE ... RETURNING` that yields the columns [`WRITE_MARKERS`] reads. The result
/// is one statement that deletes and writes the markers, and returns the number of rows deleted
/// as a single `bigint`.
pub(crate) fn delete_and_mark(gone: &str) -> String {
    format!(
        "WITH gone AS ({gone}), marked AS ({WRITE_MARKERS} RETURNING 1) \
         SELECT count(*) FROM gone"
    )
}

/// The columns of `gone` for a deleted event: the destination a `sync.*` outcome event names, and
/// NULL for any other event. `alias` is the event table's alias in the delete.
pub(crate) fn event_columns(alias: &str) -> String {
    format!(
        "{alias}.workspace_id, {alias}.person_id,          CASE WHEN {alias}.name IN ('{SYNC_SUCCEEDED_EVENT}', '{SYNC_FAILED_EVENT}')               THEN {alias}.properties ->> 'destination_id' END AS destination,          {alias}.occurred_at AS handled_at"
    )
}

/// Writes the markers of every `sync.*` event of `table` (a partition about to be dropped).
pub(crate) fn mark_partition(table: &str) -> String {
    let columns = event_columns("e");
    format!("WITH gone AS (SELECT {columns} FROM {table} e) {WRITE_MARKERS}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_marking_delete_wraps_the_delete_and_counts_its_rows() {
        let sql = delete_and_mark("DELETE FROM t RETURNING a");

        assert!(sql.starts_with("WITH gone AS (DELETE FROM t RETURNING a), marked AS ("));
        assert!(sql.ends_with("SELECT count(*) FROM gone"));
        assert!(sql.contains("ON CONFLICT DO NOTHING"));
    }

    #[test]
    fn event_columns_name_a_destination_only_for_sync_outcomes() {
        let columns = event_columns("e");

        assert!(columns.contains(SYNC_SUCCEEDED_EVENT) && columns.contains(SYNC_FAILED_EVENT));
        assert!(columns.contains("AS destination") && columns.contains("AS handled_at"));
    }
}
