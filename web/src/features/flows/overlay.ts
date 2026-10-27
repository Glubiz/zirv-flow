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

/** The badge grid of a card, in pixels. The stylesheet reads these through custom properties, so the card height below is exact. */
export const COUNT_ROW = 22;
export const COUNT_GAP = 4;
/** The grid has two columns: a short badge takes one cell, a skip reason both. */
const COUNT_COLUMNS = 2;

/** The height the count badges of one step add to its card: their grid rows plus the gap above them. */
export function countsHeight(counts: NodeTally): number {
  const short = 1 + (counts.waiting > 0 ? 1 : 0) + (counts.failed > 0 ? 1 : 0) + (counts.sent ? 1 : 0) + (counts.send_failed ? 1 : 0);
  const lines = Math.ceil(short / COUNT_COLUMNS) + (counts.skipped?.length ?? 0);
  return lines * COUNT_ROW + (lines - 1) * COUNT_GAP + COUNT_GAP;
}

/** The mark line a selected enrollment adds to a card, in pixels: its row and the gap above it. */
export const MARK_ROW = 18;
export const MARK_GAP = 2;

/**
 * The height a counted card needs on top of its usual one: its badge grid, and the mark line when an
 * enrollment marks the step. A step without counts keeps its usual height, mark or not.
 */
export function overlayExtra(counts: NodeTally | undefined, marked: boolean): number {
  if (!counts) return 0;
  return countsHeight(counts) + (marked ? MARK_ROW + MARK_GAP : 0);
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
