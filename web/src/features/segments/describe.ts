import { AGG_LABELS, OP_LABELS, type Op } from "./operators";
import type { Node } from "./ast";

interface Named {
  id: string;
  name: string;
}

export interface Names {
  lists: ReadonlyMap<string, string>;
  segments: ReadonlyMap<string, string>;
}

/** The names of the lists and segments a definition refers to; a list not loaded yet names nothing. */
export function namesOf(lists: readonly Named[] = [], segments: readonly Named[] = []): Names {
  return {
    lists: new Map(lists.map((list) => [list.id, list.name])),
    segments: new Map(segments.map((segment) => [segment.id, segment.name])),
  };
}

export function valueText(value: unknown): string {
  if (value === undefined || value === null) return "";
  if (Array.isArray(value)) return (value as unknown[]).map(valueText).join(", ");
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  return JSON.stringify(value);
}

function comparison(op: Op, value: unknown): string {
  const text = valueText(value);
  return `${OP_LABELS[op]}${text === "" ? "" : ` ${text}`}`;
}

/** What a condition says, in words, for a reader who did not build it. */
export function describeNode(node: Node, names: Names): string {
  if ("all" in node) return "All of these are true";
  if ("any" in node) return "At least one of these is true";
  if ("not" in node) return "This is not true";
  if ("trait" in node) {
    return `${node.trait} ${comparison(node.op, node.value)}`;
  }
  if ("event" in node) {
    const parts = [`Has the event ${node.event}`];
    const window = node.window;
    if (window !== undefined) {
      if ("last" in window) parts.push(`in the last ${window.last}`);
      else if ("between" in window) parts.push(`between ${window.between.join(" and ")}`);
      else parts.push(`since the date in ${window.since_trait}`);
    }
    const filters = node.where ?? [];
    if (filters.length > 0) {
      parts.push(
        `where ${filters.map((filter) => `${filter.prop} ${comparison(filter.op, filter.value)}`).join(" and ")}`,
      );
    }
    if (node.agg) {
      const agg = node.agg;
      parts.push(
        `and ${AGG_LABELS[agg.fn].toLowerCase()}${agg.prop ? ` ${agg.prop}` : ""} ${comparison(agg.op, agg.value)}`,
      );
    }
    return parts.join(" ");
  }
  if ("list" in node) return `Is on the list ${names.lists.get(node.list) ?? "(unknown list)"}`;
  if ("segment" in node) return `Is in the segment ${names.segments.get(node.segment) ?? "(unknown segment)"}`;
  if ("consent" in node) {
    const { channel, topic, status } = node.consent;
    return `Consent for ${channel}${topic ? `, topic ${topic},` : ""} is ${status.replaceAll("_", " ")}`;
  }
  return `Has a ${node.identifier} identifier`;
}
