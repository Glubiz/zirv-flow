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
  /** Messages of a send step that went out. */
  sent?: number;
  /** Messages of a send step that ended failed. */
  send_failed?: number;
  /** Messages of a send step that were skipped, by reason code. */
  skipped?: readonly { reason: string; count: number }[];
}

const COUNT_ROW = 22;
const COUNT_GAP = 4;
/** The short badges fit two to a line on a card; a skip reason takes a line of its own. */
const SHORT_PER_LINE = 2;

/** The height the count badges of one step add to its card: their lines plus the gap above them. */
export function countsHeight(counts: NodeTally): number {
  const short = 1 + (counts.waiting > 0 ? 1 : 0) + (counts.failed > 0 ? 1 : 0) + (counts.sent ? 1 : 0) + (counts.send_failed ? 1 : 0);
  const lines = Math.ceil(short / SHORT_PER_LINE) + (counts.skipped?.length ?? 0);
  return lines * COUNT_ROW + (lines - 1) * COUNT_GAP + COUNT_GAP;
}

/** What is drawn over the canvas: a path with its numbers, counts per step, marks. */
export interface Overlay {
  /** The numbers a step has on the path (a step can be reached twice), by step id. */
  steps?: ReadonlyMap<string, readonly number[]>;
  /** The wires the path takes, by edge id. */
  pathEdges?: ReadonlySet<string>;
  /** Dim every step that is not on the path. */
  dimOthers?: boolean;
  counts?: ReadonlyMap<string, NodeTally>;
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
    const between = graph.edges.filter((edge) => edge.from === stop.node && edge.to === next.node);
    // The port names the wire. Without it, a single wire between the two steps is still certain; with several
    // (two arms of a branch that share a target) any pick would be a guess, so none is lit and the steps keep their numbers.
    const found = between.find((edge) => edge.port === stop.port) ?? (between.length === 1 ? between[0] : undefined);
    if (found) edges.add(found.id);
  });
  if (path.length > 0) {
    steps.set(TRIGGER_ID, []);
    edges.add(TRIGGER_EDGE);
  }
  return { steps, pathEdges: edges, dimOthers: path.length > 0 };
}
