import type { Graph } from "./model";
import { TRIGGER_ID } from "./model";

/** How a step is marked for one enrollment. */
export interface NodeMark {
  tone: "failed" | "retried" | "skipped";
  text: string;
}

export interface NodeTally {
  entered: number;
  waiting: number;
  failed: number;
}

/** What is drawn over the canvas: a path with its numbers, counts per step and per output, marks. */
export interface Overlay {
  /** The numbers a step has on the path (a step can be reached twice), by step id. */
  steps?: ReadonlyMap<string, readonly number[]>;
  /** The wires the path takes, by edge id. */
  pathEdges?: ReadonlySet<string>;
  /** Dim every step that is not on the path. */
  dimOthers?: boolean;
  counts?: ReadonlyMap<string, NodeTally>;
  /** How many took an output, by `stepId/port`. */
  portCounts?: ReadonlyMap<string, number>;
  marks?: ReadonlyMap<string, NodeMark>;
}

export interface PathStop {
  node: string;
  port?: string | null;
}

/** The id of the wire from the trigger to the entry step. */
export const TRIGGER_EDGE = "__trigger-edge";

/** The numbers and wires of a path of steps; the trigger and its wire are on it whenever it is not empty. */
export function pathOverlay(graph: Graph, path: readonly PathStop[]): Pick<Overlay, "steps" | "pathEdges" | "dimOthers"> {
  const steps = new Map<string, number[]>();
  const edges = new Set<string>();
  path.forEach((stop, index) => {
    steps.set(stop.node, [...(steps.get(stop.node) ?? []), index + 1]);
    const next = path[index + 1];
    if (!next) return;
    const exact = graph.edges.find((edge) => edge.from === stop.node && edge.to === next.node && edge.port === stop.port);
    const any = graph.edges.find((edge) => edge.from === stop.node && edge.to === next.node);
    const found = exact ?? any;
    if (found) edges.add(found.id);
  });
  if (path.length > 0) {
    steps.set(TRIGGER_ID, []);
    edges.add(TRIGGER_EDGE);
  }
  return { steps, pathEdges: edges, dimOthers: path.length > 0 };
}
