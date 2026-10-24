import type { components } from "../../api";
import { aggNeedsProperty, aggType, operandType, shapeOf, type ValueType } from "./operators";

export type Node = components["schemas"]["SegmentNode"];
export type Definition = components["schemas"]["SegmentDefinition"];
export type Aggregate = components["schemas"]["SegmentAggregate"];
export type PropertyFilter = components["schemas"]["SegmentPropertyFilter"];
export type Window = components["schemas"]["SegmentWindow"];
export type TraitNode = components["schemas"]["SegmentTraitNode"];
export type EventNode = components["schemas"]["SegmentEventNode"];
export type ConsentNode = components["schemas"]["SegmentConsentNode"];

export type Kind = "all" | "any" | "not" | "trait" | "event" | "list" | "segment" | "consent" | "identifier";

export const KINDS: readonly Kind[] = ["all", "any", "not", "trait", "event", "list", "segment", "consent", "identifier"];

/** A position in the tree, written as the tokens of the JSON pointer the API reports. */
export type Path = readonly (string | number)[];

export function kindOf(node: Node): Kind {
  if ("all" in node) return "all";
  if ("any" in node) return "any";
  if ("not" in node) return "not";
  if ("trait" in node) return "trait";
  if ("event" in node) return "event";
  if ("list" in node) return "list";
  if ("segment" in node) return "segment";
  if ("consent" in node) return "consent";
  return "identifier";
}

export function pointerOf(path: Path): string {
  return path.map((token) => `/${token}`).join("");
}

export function samePath(left: Path, right: Path): boolean {
  return left.length === right.length && left.every((token, index) => token === right[index]);
}

export function startsWith(path: Path, prefix: Path): boolean {
  return path.length >= prefix.length && prefix.every((token, index) => token === path[index]);
}

export function defaultNode(kind: Kind): Node {
  switch (kind) {
    case "all":
      return { all: [] };
    case "any":
      return { any: [] };
    case "not":
      return { not: { trait: "", op: "eq" } };
    case "trait":
      return { trait: "", op: "eq" };
    case "event":
      return { event: "" };
    case "list":
      return { list: "" };
    case "segment":
      return { segment: "" };
    case "consent":
      return { consent: { channel: "email", status: "subscribed" } };
    case "identifier":
      return { identifier: "email" };
  }
}

/** Switching between "all" and "any" keeps the children; every other switch starts the node afresh. */
export function changeKind(node: Node, kind: Kind): Node {
  if (kind === kindOf(node)) return node;
  if ("all" in node && kind === "any") return { any: node.all };
  if ("any" in node && kind === "all") return { all: node.any };
  return defaultNode(kind);
}

/** The children of an "all" or "any" group; null for any other node. */
export function childrenOf(node: Node): Node[] | null {
  if ("all" in node) return node.all;
  if ("any" in node) return node.any;
  return null;
}

export function getAt(root: Node, path: Path): Node | undefined {
  let node: Node | undefined = root;
  let index = 0;
  while (node !== undefined && index < path.length) {
    const token = path[index];
    if (token === "not" && "not" in node) {
      node = node.not;
      index += 1;
    } else if (token === "all" || token === "any") {
      const children = childrenOf(node);
      node = children?.[Number(path[index + 1])];
      index += 2;
    } else {
      return undefined;
    }
  }
  return node;
}

/** A copy of the tree with the node at `path` replaced by `change(node)`. */
export function updateAt(node: Node, path: Path, change: (node: Node) => Node): Node {
  const [head, ...rest] = path;
  if (head === undefined) return change(node);
  if (head === "not" && "not" in node) {
    return { not: updateAt(node.not, rest, change) };
  }
  if (head === "all" || head === "any") {
    const children = childrenOf(node);
    const position = Number(rest[0]);
    if (children === null) return node;
    const next = children.map((child, at) => (at === position ? updateAt(child, rest.slice(1), change) : child));
    return "all" in node ? { all: next } : { any: next };
  }
  return node;
}

export function addChild(root: Node, group: Path, child: Node): Node {
  return updateAt(root, group, (node) => {
    if ("all" in node) return { all: [...node.all, child] };
    if ("any" in node) return { any: [...node.any, child] };
    return node;
  });
}

export function removeChild(root: Node, group: Path, position: number): Node {
  return updateAt(root, group, (node) => {
    if ("all" in node) return { all: node.all.filter((_, at) => at !== position) };
    if ("any" in node) return { any: node.any.filter((_, at) => at !== position) };
    return node;
  });
}

/** Where an issue's pointer lands: the node it names and the field inside that node. */
export interface Placement {
  path: Path;
  field: string;
}

function fieldOf(rest: readonly string[]): string {
  const [head, second, third] = rest;
  if (head === undefined) return "";
  if (head === "where") return `where.${second ?? ""}.${third ?? ""}`;
  if (head === "agg") return `agg.${second ?? ""}`;
  if (head === "consent") return `consent.${second ?? ""}`;
  if (head === "window") return "window";
  return head;
}

function unescape(token: string): string {
  return token.replaceAll("~1", "/").replaceAll("~0", "~");
}

/** The node and field a JSON pointer names in `root`, or null when it names no node of the tree. */
export function place(root: Node, pointer: string): Placement | null {
  const tokens = pointer === "" ? [] : pointer.split("/").slice(1).map(unescape);
  const path: (string | number)[] = [];
  let node: Node = root;
  let index = 0;
  while (index < tokens.length) {
    const token = tokens[index];
    if (token === "not" && "not" in node) {
      path.push("not");
      node = node.not;
      index += 1;
      continue;
    }
    if ((token === "all" || token === "any") && index + 1 < tokens.length) {
      const children = childrenOf(node);
      const position = Number(tokens[index + 1]);
      const child = children?.[position];
      if (child === undefined) return null;
      path.push(token, position);
      node = child;
      index += 2;
      continue;
    }
    break;
  }
  const rest = tokens.slice(index);
  const group = kindOf(node) === "all" || kindOf(node) === "any" || kindOf(node) === "not";
  if (group && rest.length > 0) return rest[0] === "all" || rest[0] === "any" || rest[0] === "not" ? { path, field: "" } : null;
  return { path, field: fieldOf(rest) };
}

function scalarFilled(type: ValueType, value: unknown): boolean {
  switch (type) {
    case "boolean":
      return typeof value === "boolean";
    case "number":
    case "integer":
      return typeof value === "number" && Number.isFinite(value);
    default:
      return typeof value === "string" && value !== "";
  }
}

/** Whether an operand is filled in the shape the operator takes. */
export function valueFilled(type: ValueType, op: components["schemas"]["SegmentOp"], value: unknown): boolean {
  const shape = shapeOf(type, op);
  const item = operandType(type);
  switch (shape) {
    case "none":
      return value === undefined;
    case "scalar":
      return scalarFilled(item, value);
    case "duration":
      return typeof value === "string" && value !== "";
    case "list":
      return Array.isArray(value) && value.length > 0 && value.every((entry: unknown) => scalarFilled(item, entry));
    case "pair":
      return Array.isArray(value) && value.length === 2 && value.every((entry: unknown) => scalarFilled(item, entry));
  }
}

/** The type a trait key has in the registry, by key. */
export type TraitTypes = ReadonlyMap<string, ValueType>;

/**
 * Whether every field of the definition is filled, so it can be sent for a count. It looks only at
 * what is filled; whether the values are valid is the API's answer.
 */
export function isComplete(node: Node, traits: TraitTypes, events: ReadonlyMap<string, ReadonlyMap<string, ValueType>>): boolean {
  if ("all" in node) return node.all.every((child) => isComplete(child, traits, events));
  if ("any" in node) return node.any.every((child) => isComplete(child, traits, events));
  if ("not" in node) return isComplete(node.not, traits, events);
  if ("trait" in node) {
    const type = traits.get(node.trait);
    return type !== undefined && valueFilled(type, node.op, node.value);
  }
  if ("event" in node) {
    if (node.event === "") return false;
    const properties = events.get(node.event);
    if (properties === undefined) return false;
    const filters = (node.where ?? []).every((filter) => {
      const type = properties.get(filter.prop);
      return type !== undefined && valueFilled(type, filter.op, filter.value);
    });
    if (!filters) return false;
    const agg = node.agg;
    if (agg !== undefined) {
      if (aggNeedsProperty(agg.fn) && (agg.prop === undefined || agg.prop === "")) return false;
      if (!valueFilled(aggType(agg.fn), agg.op, agg.value)) return false;
    }
    const window = node.window;
    if (window !== undefined) {
      if ("last" in window) return window.last !== "";
      if ("between" in window) return window.between.length === 2 && window.between.every((entry) => entry !== "");
      return window.since_trait !== "";
    }
    return true;
  }
  if ("list" in node) return node.list !== "";
  if ("segment" in node) return node.segment !== "";
  if ("consent" in node) return (node.consent.channel ?? "") !== "";
  // A policy-only leaf: the segment builder does not make or complete it.
  if ("messages" in node) return false;
  const kind = node.identifier;
  return kind !== "" && kind !== "external:";
}

/** The saved form of a draft: the tree with the definition version beside its key. */
export function toDefinition(root: Node): Definition {
  return { ...root, v: 1 };
}

/** The tree of a saved definition, without its version. */
export function fromDefinition(definition: Definition): Node {
  const copy: Record<string, unknown> = { ...definition };
  delete copy["v"];
  return copy as unknown as Node;
}

function deepEqual(left: unknown, right: unknown): boolean {
  if (left === right) return true;
  if (Array.isArray(left) || Array.isArray(right)) {
    return (
      Array.isArray(left) &&
      Array.isArray(right) &&
      left.length === right.length &&
      left.every((entry: unknown, at) => deepEqual(entry, right[at]))
    );
  }
  if (typeof left !== "object" || typeof right !== "object" || left === null || right === null) return false;
  const a = left as Record<string, unknown>;
  const b = right as Record<string, unknown>;
  const keys = Object.keys(a).filter((key) => a[key] !== undefined);
  const others = Object.keys(b).filter((key) => b[key] !== undefined);
  return keys.length === others.length && keys.every((key) => deepEqual(a[key], b[key]));
}

/** Whether two trees say the same thing: the order of a node's keys does not matter, the order of a list does. */
export function sameDefinition(left: Node, right: Node): boolean {
  return deepEqual(left, right);
}
