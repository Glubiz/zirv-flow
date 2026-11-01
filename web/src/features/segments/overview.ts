import type { RelatedGroup } from "../../ui";
import { childrenOf, type Node } from "./ast";
import { describeNode, type Names } from "./describe";

/** The definition as one sentence for someone who did not build it. */
export function describeDefinition(root: Node, names: Names): string {
  const children = childrenOf(root);
  if (children === null) {
    return `People for whom this holds: ${describeNode(root, names)}.`;
  }
  if (children.length === 0) return "Everyone.";
  const lead = "all" in root ? "People who match all of these" : "People who match at least one of these";
  return `${lead}: ${children.map((child) => describeNode(child, names)).join("; ")}.`;
}

interface Dependencies {
  traits?: string[];
  events?: string[];
  lists?: string[];
  segments?: string[];
  consent?: [string, string | null][];
}

interface Named {
  id: string;
  name: string;
}

/** What a segment reads, from the saved version's `deps`; a group with nothing in it is left empty. */
export function builtFrom(
  deps: unknown,
  workspaceId: string,
  lists: readonly Named[],
  segments: readonly Named[],
  selfId: string,
): RelatedGroup[] {
  const read = (deps ?? {}) as Dependencies;
  const nameOf = (rows: readonly Named[], id: string) => rows.find((row) => row.id === id)?.name ?? id;
  const items: RelatedGroup["items"] = [
    ...(read.traits ?? []).map((key) => ({ key: `trait:${key}`, label: key, meta: "field" })),
    ...(read.events ?? []).map((key) => ({ key: `event:${key}`, label: key, meta: "event" })),
    ...(read.lists ?? []).map((id) => ({ key: `list:${id}`, label: nameOf(lists, id), to: `/w/${workspaceId}/lists/${id}`, meta: "list" })),
    ...(read.segments ?? [])
      .filter((id) => id !== selfId)
      .map((id) => ({ key: `segment:${id}`, label: nameOf(segments, id), to: `/w/${workspaceId}/segments/${id}`, meta: "segment" })),
    ...(read.consent ?? []).map(([channel, topic]) => ({
      key: `consent:${channel}:${topic ?? ""}`,
      label: topic ? `${channel}, ${topic}` : channel,
      meta: "consent",
    })),
  ];
  return [{ label: "Built from", items }];
}

interface WhereUsed {
  flows: { flow_id: string; name: string; stage: string }[];
  segments: { segment_id: string; name: string }[];
}

/** The flows and segments that use a segment; a group with nothing in it is left empty. */
export function usedBy(used: WhereUsed | null, workspaceId: string): RelatedGroup[] {
  return [
    {
      label: "Used by flows",
      items: (used?.flows ?? []).map((flow) => ({
        key: `flow:${flow.flow_id}`,
        label: flow.name,
        to: `/w/${workspaceId}/flows/${flow.flow_id}`,
        meta: flow.stage === "live" ? "Live" : "Draft",
      })),
    },
    {
      label: "Used by segments",
      items: (used?.segments ?? []).map((segment) => ({
        key: `segment:${segment.segment_id}`,
        label: segment.name,
        to: `/w/${workspaceId}/segments/${segment.segment_id}`,
      })),
    },
  ];
}
