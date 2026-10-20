import { portLabel, TRIGGER_ID, type FlowIssue, type Graph } from "./model";

/** What each issue code means, in plain words. Every code the API can report has an entry. */
const MESSAGES: Record<FlowIssue["code"], string> = {
  unsupported_version: "This flow was written in a format this version cannot read.",
  too_many_nodes: "The flow has more steps than a flow may have.",
  too_many_edges: "The flow has more connections than a flow may have.",
  invalid_id: "A step or connection has an id that is empty, too long or uses characters that are not allowed.",
  duplicate_node_id: "Two steps share the same id.",
  duplicate_edge_id: "Two connections share the same id.",
  unknown_node: "A connection leads to or from a step that is not in the flow.",
  unknown_port: "A connection starts from an output the step does not have.",
  port_wired_twice: "An output is connected more than once. Each output leads to exactly one step.",
  port_not_wired: "An output is not connected to a next step.",
  no_entry: "No step is the start of the flow. Every step has something leading into it, so there is a loop.",
  multiple_entries: "More than one step has nothing leading into it. Connect the extra steps, or delete them.",
  cycle: "The steps lead back into themselves. A flow cannot loop.",
  unreachable: "No path from the start leads to this step.",
  invalid_duration: "The time is not valid. Use at least 1 second and at most a year.",
  invalid_time: "The time is not valid. Use a time of day such as 09:30, or a date and time.",
  invalid_weekdays: "The days of the week are not valid.",
  invalid_event_name: "Choose the event to wait for.",
  invalid_filter: "A filter on the event is not valid.",
  no_conditions: "A branch needs at least one condition.",
  too_many_conditions: "A branch has more conditions than it may have.",
  invalid_condition: "A condition is not valid or is not filled in.",
  too_few_arms: "A random split needs at least two arms.",
  too_many_arms: "A random split has more arms than it may have.",
  invalid_weight: "An arm needs a weight from 1 to 100.",
  weights_not_100: "The weights of the arms must add up to 100.",
  invalid_url: "The web address is not valid. Use an http or https address.",
  invalid_trait_key: "A field name is empty or not valid.",
  invalid_trait_value: "The value is not valid for this change.",
  too_many_trait_keys: "The webhook sends more fields than it may.",
  invalid_cron: "The schedule is not a valid cron expression.",
  invalid_timezone: "The time zone is not a valid time zone name.",
  exit_on_goal_without_goal: "Leaving the flow on the goal needs a goal. Add one, or turn that off.",
  goal_criterion_without_goal: "An exit rule refers to the goal, but the flow has no goal.",
  leaves_segment_without_segment_trigger: "Leaving the segment can only end a flow that is started by a segment.",
  too_many_exit_criteria: "The flow has more exit rules than it may have.",
  unknown_segment: "This refers to a segment that does not exist. Choose one.",
  unknown_list: "This refers to a list that does not exist. Choose one.",
  unknown_trait: "This refers to a field that does not exist in the workspace.",
  unknown_event: "This refers to an event the workspace has no schema for.",
  unknown_property: "This refers to a property the event does not have.",
  unknown_topic: "This refers to a topic that does not exist.",
  webhook_url_refused: "This address is not allowed. Webhooks cannot call private or local addresses.",
  webhook_secret_missing: "The workspace has no webhook signing secret. An admin can create one in the workspace settings.",
};

/** The text for an issue, with the output it concerns named when it is about one. */
export function issueText(issue: FlowIssue, graph?: Graph): string {
  const known = (MESSAGES as Record<string, string | undefined>)[issue.code];
  const base = known ?? `This flow has a problem (${issue.code}).`;
  if (issue.code === "no_entry" && graph !== undefined && graph.nodes.length === 0) {
    return "The flow has no steps yet. Add the first step after the trigger.";
  }
  const port = issue.at?.startsWith("/ports/") ? issue.at.slice("/ports/".length) : null;
  if (issue.code === "port_not_wired" && port !== null) {
    const node = graph?.nodes.find((candidate) => candidate.id === issue.node);
    return `The ${node ? portLabel(node, port) : port} output is not connected to a next step.`;
  }
  return base;
}

export type Section = "trigger" | "reentry" | "goal" | "exit";

export const SECTIONS: readonly { id: Section; label: string }[] = [
  { id: "trigger", label: "Trigger" },
  { id: "reentry", label: "Re-entry" },
  { id: "goal", label: "Goal" },
  { id: "exit", label: "Exit criteria" },
];

/** The settings section a pointer into the settings object lands in. */
function sectionOf(at: string | undefined): Section | null {
  if (at === undefined) return null;
  if (at.startsWith("/trigger")) return "trigger";
  if (at.startsWith("/reentry")) return "reentry";
  if (at.startsWith("/goal") || at === "/exit_on_goal") return "goal";
  if (at.startsWith("/exit_criteria")) return "exit";
  return null;
}

export interface PlacedIssues {
  byNode: Map<string, FlowIssue[]>;
  byEdge: Map<string, FlowIssue[]>;
  bySection: Map<Section, FlowIssue[]>;
  /** Issues about the flow as a whole, with no step, connection or section to show them on. */
  general: FlowIssue[];
}

function push<K>(map: Map<K, FlowIssue[]>, key: K, issue: FlowIssue) {
  map.set(key, [...(map.get(key) ?? []), issue]);
}

/** Puts each issue on its step, its connection or its settings section. */
export function placeIssues(issues: readonly FlowIssue[]): PlacedIssues {
  const placed: PlacedIssues = { byNode: new Map(), byEdge: new Map(), bySection: new Map(), general: [] };
  for (const issue of issues) {
    if (issue.node !== undefined) {
      push(placed.byNode, issue.node, issue);
    } else if (issue.edge !== undefined) {
      push(placed.byEdge, issue.edge, issue);
    } else {
      const section = sectionOf(issue.at);
      if (section === null) {
        placed.general.push(issue);
      } else {
        push(placed.bySection, section, issue);
        // The trigger node on the canvas carries the issues of its section.
        if (section === "trigger") push(placed.byNode, TRIGGER_ID, issue);
      }
    }
  }
  return placed;
}

/** The pointer inside a branch condition, relative to that condition, for the builder to place. */
export function conditionIssues(issues: readonly FlowIssue[], index: number): { pointer: string; code: string }[] {
  const prefix = `/conditions/${index}`;
  return issues.flatMap((issue) => {
    const at = issue.at;
    if (at === undefined || !(at === prefix || at.startsWith(`${prefix}/`))) return [];
    return [{ pointer: at.slice(prefix.length), code: issue.code }];
  });
}

/** Issues that name this field of a step, by the start of their pointer. */
export function issuesAt(issues: readonly FlowIssue[] | undefined, prefix: string): FlowIssue[] {
  return (issues ?? []).filter((issue) => issue.at === prefix || issue.at?.startsWith(`${prefix}/`));
}
