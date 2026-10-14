//! Segment references form a graph (a definition may name another segment), and a save must not
//! close a cycle in it.

use hashbrown::{HashMap, HashSet};
use sqlx::PgConnection;
use sqlx::types::Json;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::StoreError;

/// A segment named by its id, for an error that has to say which segments it means.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentRef {
    pub id: Uuid,
    pub name: String,
}

/// The ids a stored `deps` value lists under `segments`.
pub(super) fn references_of(deps: &serde_json::Value) -> Vec<Uuid> {
    deps.get("segments")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str()?.parse().ok())
        .collect()
}

/// A path that leaves `start` along `edges` and comes back to it, as the nodes in order with
/// `start` first and last; `None` when `start` is on no cycle. The graph may hold other cycles,
/// which are not reported. The search is iterative, so a long chain cannot overflow the stack.
pub(super) fn find_cycle(start: Uuid, edges: &HashMap<Uuid, Vec<Uuid>>) -> Option<Vec<Uuid>> {
    let mut visited: HashSet<Uuid> = HashSet::new();
    visited.insert(start);
    let mut stack: Vec<(Uuid, usize)> = vec![(start, 0)];
    while let Some(&(node, next)) = stack.last() {
        let child = edges.get(&node).and_then(|children| children.get(next));
        let Some(&child) = child else {
            stack.pop();
            continue;
        };
        if let Some(top) = stack.last_mut() {
            top.1 += 1;
        }
        if child == start {
            let mut path: Vec<Uuid> = stack.iter().map(|(node, _)| *node).collect();
            path.push(start);
            return Some(path);
        }
        if visited.insert(child) {
            stack.push((child, 0));
        }
    }
    None
}

/// Checks that giving `segment` the references `references` closes no cycle. The edges are those
/// of every live segment's current version, with this segment's own replaced. The caller holds
/// the workspace's segments lock, so no concurrent save can add the other half of a cycle.
pub(super) async fn check_no_cycle(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    segment: Uuid,
    references: &[Uuid],
) -> Result<Option<Vec<SegmentRef>>, StoreError> {
    let rows: Vec<(Uuid, String, Option<Json<serde_json::Value>>)> = sqlx::query_as(
        "SELECT s.id, s.name, v.deps FROM segments s \
         JOIN segment_versions v ON v.segment_id = s.id AND v.version = s.current_version \
         WHERE s.workspace_id = $1 AND s.archived_at IS NULL",
    )
    .bind(workspace.as_uuid())
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let mut names: HashMap<Uuid, String> = HashMap::new();
    let mut edges: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for (id, name, deps) in rows {
        names.insert(id, name);
        let mut children = deps.map_or_else(Vec::new, |Json(deps)| references_of(&deps));
        children.sort();
        edges.insert(id, children);
    }
    let mut own = references.to_vec();
    own.sort();
    own.dedup();
    edges.insert(segment, own);
    Ok(find_cycle(segment, &edges).map(|path| {
        path.into_iter()
            .map(|id| SegmentRef {
                name: names.get(&id).cloned().unwrap_or_default(),
                id,
            })
            .collect()
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn graph(edges: &[(u128, &[u128])]) -> HashMap<Uuid, Vec<Uuid>> {
        edges
            .iter()
            .map(|(from, to)| (id(*from), to.iter().map(|n| id(*n)).collect()))
            .collect()
    }

    #[test]
    fn a_self_reference_is_a_cycle_of_one() {
        let path = find_cycle(id(1), &graph(&[(1, &[1])])).unwrap();

        assert_eq!(path, [id(1), id(1)]);
    }

    #[test]
    fn a_direct_and_a_transitive_cycle_come_back_in_path_order() {
        let direct = find_cycle(id(1), &graph(&[(1, &[2]), (2, &[1])])).unwrap();
        let transitive = find_cycle(id(1), &graph(&[(1, &[2]), (2, &[3]), (3, &[1])])).unwrap();

        assert_eq!(direct, [id(1), id(2), id(1)]);
        assert_eq!(transitive, [id(1), id(2), id(3), id(1)]);
    }

    #[test]
    fn a_graph_without_a_way_back_has_no_cycle_even_when_it_branches_and_rejoins() {
        let diamond = graph(&[(1, &[2, 3]), (2, &[4]), (3, &[4]), (4, &[])]);

        assert_eq!(find_cycle(id(1), &diamond), None);
        assert_eq!(find_cycle(id(9), &diamond), None);
    }

    #[test]
    fn a_cycle_that_does_not_pass_through_the_start_is_not_reported() {
        let other = graph(&[(1, &[2]), (2, &[3]), (3, &[2])]);

        assert_eq!(find_cycle(id(1), &other), None);
    }

    #[test]
    fn a_long_chain_does_not_overflow_the_stack() {
        let mut edges = HashMap::new();
        for n in 1..200_000u128 {
            edges.insert(id(n), vec![id(n + 1)]);
        }
        edges.insert(id(200_000), vec![id(1)]);

        assert_eq!(find_cycle(id(1), &edges).unwrap().len(), 200_001);
    }

    #[test]
    fn the_references_are_read_from_the_segments_of_the_deps() {
        let deps = json!({"traits": [], "segments": [id(7).to_string(), "not an id"]});

        assert_eq!(references_of(&deps), [id(7)]);
        assert!(references_of(&json!({})).is_empty());
    }
}
