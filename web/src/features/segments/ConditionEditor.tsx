import { describeError, type DefinitionIssue } from "../../api";
import { ErrorState, LoadingState } from "../../ui";
import { place, type BuilderMode, type Node } from "./ast";
import { Builder } from "./Builder";
import type { PlacedIssue } from "./builderContext";
import { useCatalog, useEventTypes, type DimensionDef } from "./catalog";
import { describeNode, namesOf } from "./describe";

export type ConditionNode = Node;

/** The names of the events a condition reads. */
function eventNames(node: Node): string[] {
  if ("all" in node) return node.all.flatMap(eventNames);
  if ("any" in node) return node.any.flatMap(eventNames);
  if ("not" in node) return eventNames(node.not);
  if ("event" in node) return [node.event];
  return [];
}

interface ConditionEditorProps {
  workspaceId: string;
  /** The condition to edit: one node of the segment AST, without the `v` of a whole definition. */
  value: Node;
  onChange: (next: Node) => void;
  readOnly?: boolean;
  /** A segment condition (the default) or a consent policy rule, which also offers the policy-only leaves. */
  mode?: BuilderMode;
  /** The workspace dimensions a policy rule can filter sent messages by. */
  dimensions?: readonly DimensionDef[];
  /** Problems with the condition; each pointer is relative to `value`, as in a segment definition. */
  issues?: readonly DefinitionIssue[];
}

/**
 * Edits one segment condition with the segments builder, for a page that holds a condition of its
 * own, such as the branch of a flow. It loads the fields, events, lists and segments to pick from.
 */
export function ConditionEditor({ workspaceId, value, onChange, readOnly = false, issues = [], mode = "segment", dimensions = [] }: ConditionEditorProps) {
  const catalog = useCatalog(workspaceId);
  const eventTypes = useEventTypes(workspaceId, eventNames(value));
  if (catalog.status === "loading") return <LoadingState label="Loading the fields, events and lists to choose from" />;
  if (catalog.status === "error") return <ErrorState {...describeError(catalog.error)} onRetry={catalog.reload} />;

  const placed: PlacedIssue[] = [];
  for (const issue of issues) {
    const where = place(value, issue.pointer);
    if (where !== null) placed.push({ ...issue, path: where.path, field: where.field });
  }
  return (
    <Builder
      root={value}
      readOnly={readOnly}
      mode={mode}
      dimensions={dimensions}
      selfId={null}
      catalog={catalog.catalog}
      eventTypes={eventTypes}
      issues={placed}
      onEdit={(next) => onChange(next)}
    />
  );
}

/** What a condition says, in words. Lists and segments it names show by name only when given. */
export function describeCondition(
  value: Node,
  named: { lists?: readonly { id: string; name: string }[]; segments?: readonly { id: string; name: string }[] } = {},
): string {
  return describeNode(value, namesOf(named.lists, named.segments));
}
