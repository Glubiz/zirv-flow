import { entryOf, portsOf, TRIGGER_ID, type FlowNode, type Graph } from "./model";

const NODE_WIDTH = 220;
const GAP_X = 40;
const GAP_Y = 56;

interface Placed {
  id: string;
  x: number;
  y: number;
  width: number;
  height: number;
  /** Rows from the trigger; the trigger is 0 and the entry node 1. */
  depth: number;
  /** Where the node comes in reading order: top to bottom, left to right. The trigger is 0. */
  order: number;
  /** False for a step the entry node does not lead to, placed after the rest. */
  reachable: boolean;
}

function sizeOf(node: FlowNode | null): { width: number; height: number } {
  if (node === null) return { width: NODE_WIDTH, height: 84 };
  if (node.type === "exit") return { width: NODE_WIDTH, height: 72 };
  return { width: NODE_WIDTH, height: portsOf(node).length > 1 ? 120 : 92 };
}

interface Layout {
  placed: Map<string, Placed>;
  /** Every node id, trigger first, in reading order. */
  order: string[];
  /** The ids of each row, left to right. */
  rows: string[][];
}

/**
 * A layered top-down layout. A node sits one row below the lowest row of anything that leads into
 * it, and the nodes of a row follow the order in which a walk from the entry node, taking each port
 * in turn, first reaches them. The graph carries no positions, so this runs on every change.
 */
export function layout(graph: Graph): Layout {
  const byId = new Map(graph.nodes.map((node) => [node.id, node]));
  const targets = (node: FlowNode): string[] =>
    portsOf(node).flatMap((port) => {
      const edge = graph.edges.find((candidate) => candidate.from === node.id && candidate.port === port);
      return edge && byId.has(edge.to) ? [edge.to] : [];
    });

  const entry = entryOf(graph);
  const depth = new Map<string, number>();
  const visit = new Map<string, number>();

  if (entry) {
    // The walk order: depth first, each port in turn.
    const stack = [entry.id];
    while (stack.length > 0) {
      const id = stack.pop() as string;
      if (visit.has(id)) continue;
      visit.set(id, visit.size);
      const node = byId.get(id);
      if (node) stack.push(...targets(node).reverse());
    }
    // The rows: the longest path from the entry, bounded so a cycle cannot loop for ever.
    depth.set(entry.id, 1);
    for (let pass = 0; pass <= graph.nodes.length; pass += 1) {
      let changed = false;
      for (const [id, row] of [...depth]) {
        const node = byId.get(id);
        if (!node) continue;
        for (const target of targets(node)) {
          if ((depth.get(target) ?? 0) < row + 1 && row + 1 <= graph.nodes.length + 1) {
            depth.set(target, row + 1);
            changed = true;
          }
        }
      }
      if (!changed) break;
    }
  }

  const deepest = Math.max(0, ...depth.values());
  const rowsById = new Map<number, string[]>();
  rowsById.set(0, [TRIGGER_ID]);
  const reachable = [...depth.keys()].sort((left, right) => (visit.get(left) ?? 0) - (visit.get(right) ?? 0));
  for (const id of reachable) {
    const row = depth.get(id) as number;
    rowsById.set(row, [...(rowsById.get(row) ?? []), id]);
  }
  const loose = graph.nodes.filter((node) => !depth.has(node.id)).map((node) => node.id);
  if (loose.length > 0) rowsById.set(deepest + 1, loose);

  const placed = new Map<string, Placed>();
  const rows: string[][] = [];
  const order: string[] = [];
  let y = 0;
  const lastRow = Math.max(...rowsById.keys());
  for (let row = 0; row <= lastRow; row += 1) {
    const ids = rowsById.get(row);
    if (!ids) continue;
    rows.push(ids);
    const sizes = ids.map((id) => sizeOf(id === TRIGGER_ID ? null : (byId.get(id) ?? null)));
    const total = sizes.reduce((sum, size) => sum + size.width, 0) + GAP_X * (ids.length - 1);
    let x = -total / 2;
    const tallest = Math.max(...sizes.map((size) => size.height));
    ids.forEach((id, position) => {
      const size = sizes[position] as { width: number; height: number };
      placed.set(id, {
        id,
        x,
        y,
        width: size.width,
        height: size.height,
        depth: row,
        order: order.length,
        reachable: id === TRIGGER_ID || depth.has(id),
      });
      order.push(id);
      x += size.width + GAP_X;
    });
    y += tallest + GAP_Y;
  }
  return { placed, order, rows };
}
