import type { components } from "../../api";

export type Graph = components["schemas"]["FlowGraph"];
export type FlowNode = components["schemas"]["FlowNode"];
type FlowEdge = components["schemas"]["FlowEdge"];
export type Settings = components["schemas"]["FlowSettings"];
export type FlowIssue = components["schemas"]["FlowIssue"];
export type FlowDetail = components["schemas"]["FlowDetailBody"];
export type Condition = components["schemas"]["SegmentNode"];
export type NodeType = FlowNode["type"];

/** The id of the trigger node the canvas draws in front of the entry node. It is not in the graph. */
export const TRIGGER_ID = "__trigger";

export const MAX_ARMS = 10;
export const MIN_ARMS = 2;
export const MAX_CONDITIONS = 10;
export const MAX_DURATION_SECONDS = 365 * 24 * 60 * 60;

/** The id the API reads as "none chosen yet": a placeholder that validation reports as unknown. */
export const NIL_UUID = "00000000-0000-0000-0000-000000000000";

export interface Draft {
  name: string;
  graph: Graph;
  settings: Settings;
}

interface KindInfo {
  type: NodeType;
  /** The kind in the add-a-step menu and above the title of a node. */
  label: string;
  /** What the kind does. */
  hint: string;
}

/** Every kind of step, in the order the add menu offers them. */
export const KINDS: readonly KindInfo[] = [
  { type: "wait.duration", label: "Wait", hint: "Pause for a set time" },
  { type: "wait.until_time", label: "Wait until a time", hint: "Pause until a time of day or a date" },
  { type: "wait.until_event", label: "Wait for an event", hint: "Pause until something happens, or time runs out" },
  { type: "branch", label: "Branch", hint: "Send people down a path by a condition" },
  { type: "split.random", label: "Random split", hint: "Send people down a path by chance" },
  { type: "action.webhook", label: "Webhook", hint: "Call a web address" },
  { type: "action.send", label: "Send an email", hint: "Send a published template" },
  { type: "action.sync", label: "Sync a profile", hint: "Update or remove the person in an external system" },
  { type: "action.update_trait", label: "Update a trait", hint: "Set, add to or clear a field" },
  { type: "action.list", label: "Update a list", hint: "Add people to a list or remove them" },
  { type: "goal", label: "Goal", hint: "Record a conversion" },
  { type: "exit", label: "Exit", hint: "End the flow for the person" },
];

export function kindLabel(type: NodeType): string {
  return KINDS.find((kind) => kind.type === type)?.label ?? type;
}

/** The output ports of a step, in the order the canvas draws them. */
export function portsOf(node: FlowNode): string[] {
  switch (node.type) {
    case "wait.duration":
    case "wait.until_time":
    case "action.webhook":
    case "action.sync":
    case "action.send":
    case "action.update_trait":
    case "action.list":
      return ["next"];
    case "goal":
      return node.exit ? [] : ["next"];
    case "wait.until_event":
      return ["event", "timeout"];
    case "branch":
      return [...node.conditions.map((_, index) => `case_${index}`), "else"];
    case "split.random":
      return node.arms.map((_, index) => `arm_${index}`);
    case "exit":
      return [];
  }
}

/** The name of a port in words: "Yes" and "No" for one condition, "Case 2", "Else", "Arm 1 (30%)". */
export function portLabel(node: FlowNode, port: string): string {
  if (port === "next") return "Next";
  if (port === "event") return "Arrived";
  if (port === "timeout") return "Timed out";
  if (port === "else") return node.type === "branch" && node.conditions.length === 1 ? "No" : "Else";
  if (port.startsWith("case_")) {
    const index = Number(port.slice(5));
    return node.type === "branch" && node.conditions.length === 1 ? "Yes" : `Case ${index + 1}`;
  }
  if (port.startsWith("arm_")) {
    const index = Number(port.slice(4));
    const weight = node.type === "split.random" ? node.arms[index]?.weight : undefined;
    return weight === undefined ? `Arm ${index + 1}` : `Arm ${index + 1} (${weight}%)`;
  }
  return port;
}

export function edgeFrom(graph: Graph, nodeId: string, port: string): FlowEdge | undefined {
  return graph.edges.find((edge) => edge.from === nodeId && edge.port === port);
}

/** The first node no edge leads to; the trigger leads into it. */
export function entryOf(graph: Graph): FlowNode | undefined {
  return graph.nodes.find((node) => graph.edges.every((edge) => edge.to !== node.id));
}

function nextNumber(prefix: string, ids: readonly string[]): string {
  let highest = 0;
  for (const id of ids) {
    if (id.startsWith(prefix)) {
      const value = Number(id.slice(prefix.length));
      if (Number.isInteger(value) && value > highest) highest = value;
    }
  }
  return `${prefix}${highest + 1}`;
}

/** A step of `type` with the fields a new step starts with. Nothing is wired. */
function blankNode(type: NodeType, id: string): FlowNode {
  switch (type) {
    case "wait.duration":
      return { id, type, seconds: 86_400 };
    case "wait.until_time":
      return { id, type, when: { kind: "time_of_day", time: "09:00" } };
    case "wait.until_event":
      return { id, type, event: "", timeout_seconds: 259_200 };
    case "branch":
      return { id, type, conditions: [{ trait: "", op: "eq" }] };
    case "split.random":
      return { id, type, arms: [{ weight: 50 }, { weight: 50 }] };
    case "action.webhook":
      return { id, type, url: "", trait_keys: [] };
    case "action.sync":
      return { id, type, destination: NIL_UUID, operation: "upsert", trait_keys: [] };
    case "action.send":
      return { id, type, template: NIL_UUID, destination: NIL_UUID };
    case "action.update_trait":
      return { id, type, key: "", change: { op: "set", value: "" } };
    case "action.list":
      return { id, type, list_id: NIL_UUID, change: "add" };
    case "goal":
      return { id, type };
    case "exit":
      return { id, type };
  }
}

export function addNode(graph: Graph, type: NodeType): { graph: Graph; id: string } {
  const id = nextNumber(
    "n",
    graph.nodes.map((node) => node.id),
  );
  return { graph: { ...graph, nodes: [...graph.nodes, blankNode(type, id)] }, id };
}

/** Wires `port` of `from` to `to`, replacing any earlier wire from that port. */
export function wire(graph: Graph, from: string, port: string, to: string): Graph {
  const kept = graph.edges.filter((edge) => !(edge.from === from && edge.port === port));
  const id = nextNumber(
    "e",
    graph.edges.map((edge) => edge.id),
  );
  return { ...graph, edges: [...kept, { id, from, port, to }] };
}

/**
 * The wire a drag on the canvas asks for, or null when it asks for none: a reader cannot wire, the
 * trigger is wired by the flow itself and nothing leads into it, and a step cannot lead to itself.
 */
export function connectionWire(
  connection: { source: string; sourceHandle?: string | null; target: string },
  editable: boolean,
): { from: string; port: string; to: string } | null {
  if (!editable || !connection.sourceHandle) return null;
  if (connection.source === TRIGGER_ID || connection.target === TRIGGER_ID || connection.source === connection.target) return null;
  return { from: connection.source, port: connection.sourceHandle, to: connection.target };
}

export function unwire(graph: Graph, edgeId: string): Graph {
  return { ...graph, edges: graph.edges.filter((edge) => edge.id !== edgeId) };
}

/** Removes a step and every wire into or out of it. */
export function removeNode(graph: Graph, id: string): Graph {
  return {
    ...graph,
    nodes: graph.nodes.filter((node) => node.id !== id),
    edges: graph.edges.filter((edge) => edge.from !== id && edge.to !== id),
  };
}

/** Replaces a step with a changed copy. Wires from a port the copy no longer has are dropped. */
export function replaceNode(graph: Graph, next: FlowNode): Graph {
  const ports = new Set(portsOf(next));
  return {
    ...graph,
    nodes: graph.nodes.map((node) => (node.id === next.id ? next : node)),
    edges: graph.edges.filter((edge) => edge.from !== next.id || ports.has(edge.port)),
  };
}

/**
 * Replaces a step with a copy that lost the `index`th of its numbered ports (a case or an arm): the
 * wire of that port goes and the wires of the ports after it move up by one.
 */
export function removeIndexedPort(graph: Graph, next: FlowNode, prefix: "case_" | "arm_", index: number): Graph {
  const edges: FlowEdge[] = [];
  for (const edge of graph.edges) {
    if (edge.from !== next.id || !edge.port.startsWith(prefix)) {
      edges.push(edge);
      continue;
    }
    const at = Number(edge.port.slice(prefix.length));
    if (at === index) continue;
    edges.push(at > index ? { ...edge, port: `${prefix}${at - 1}` } : edge);
  }
  return replaceNode({ ...graph, edges }, next);
}

export function sameJson(left: unknown, right: unknown): boolean {
  return JSON.stringify(left) === JSON.stringify(right);
}

export type DurationUnit = "second" | "minute" | "hour" | "day" | "week";

const UNITS: readonly { name: DurationUnit; seconds: number }[] = [
  { name: "week", seconds: 604_800 },
  { name: "day", seconds: 86_400 },
  { name: "hour", seconds: 3_600 },
  { name: "minute", seconds: 60 },
  { name: "second", seconds: 1 },
];

export const DURATION_UNITS: readonly DurationUnit[] = ["second", "minute", "hour", "day", "week"];

export function unitSeconds(unit: DurationUnit): number {
  return UNITS.find((entry) => entry.name === unit)?.seconds ?? 1;
}

/** The largest unit that divides `seconds` evenly, so 172800 is 2 days and 90 is 90 seconds. */
export function unitFor(seconds: number): DurationUnit {
  if (seconds <= 0) return "day";
  return UNITS.find((entry) => seconds % entry.seconds === 0)?.name ?? "second";
}

/** "1 day", "2 hours", "90 seconds". */
export function durationText(seconds: number): string {
  if (seconds <= 0) return "0 seconds";
  const unit = unitFor(seconds);
  const amount = seconds / unitSeconds(unit);
  return `${amount} ${unit}${amount === 1 ? "" : "s"}`;
}
