import { describeCondition } from "../segments";
import { durationText, NIL_UUID, type FlowNode, type Settings } from "./model";

export interface Names {
  lists: readonly { id: string; name: string }[];
  segments: readonly { id: string; name: string }[];
}

const NO_NAMES: Names = { lists: [], segments: [] };

interface Words {
  /** The headline of a node. */
  title: string;
  /** The one line under it. */
  detail: string;
}

const WEEKDAYS: Record<string, string> = {
  mon: "Mon",
  tue: "Tue",
  wed: "Wed",
  thu: "Thu",
  fri: "Fri",
  sat: "Sat",
  sun: "Sun",
};

function segmentName(id: string, names: Names): string {
  if (id === NIL_UUID) return "a segment";
  return names.segments.find((segment) => segment.id === id)?.name ?? "a segment";
}

function listName(id: string, names: Names): string {
  if (id === NIL_UUID) return "Choose a list";
  return names.lists.find((list) => list.id === id)?.name ?? "A list";
}

function hostOf(url: string): string {
  if (url.trim() === "") return "Choose a web address";
  try {
    const parsed = new URL(url);
    return `${parsed.host}${parsed.pathname === "/" ? "" : parsed.pathname}`;
  } catch {
    return url;
  }
}

function valueText(value: unknown): string {
  if (typeof value === "string") return value === "" ? "…" : value;
  return value === undefined ? "…" : JSON.stringify(value);
}

/** What a step says in a title and a line, for the node on the canvas. */
export function wordsFor(node: FlowNode, names: Names = NO_NAMES): Words {
  switch (node.type) {
    case "wait.duration":
      return { title: durationText(node.seconds), detail: "Then continue" };
    case "wait.until_time": {
      if (node.when.kind === "date_time") return { title: node.when.at.replace("T", " "), detail: "A date and time" };
      const days = node.when.weekdays ?? [];
      return {
        title: node.when.time,
        detail: days.length === 0 ? "Every day" : days.map((day) => WEEKDAYS[day] ?? day).join(", "),
      };
    }
    case "wait.until_event":
      return {
        title: node.event === "" ? "Choose an event" : node.event,
        detail: `Up to ${durationText(node.timeout_seconds)}`,
      };
    case "branch": {
      const first = node.conditions[0];
      const blank = first !== undefined && (("trait" in first && first.trait === "") || ("event" in first && first.event === ""));
      const title = first ? (blank ? "Choose a condition" : describeCondition(first, names)) : "No conditions";
      const count = node.conditions.length;
      return {
        title: count > 1 ? `${title} and ${count - 1} more` : title,
        detail: `${count} condition${count === 1 ? "" : "s"}, else ${count === 1 ? "No" : "Else"}`,
      };
    }
    case "split.random":
      return { title: `${node.arms.length} arms`, detail: node.arms.map((arm) => `${arm.weight}%`).join(" / ") };
    case "action.webhook": {
      const count = node.trait_keys?.length ?? 0;
      return { title: hostOf(node.url), detail: count === 0 ? "Sends the standard envelope" : `Sends ${count} trait${count === 1 ? "" : "s"}` };
    }
    case "action.sync": {
      const count = node.trait_keys?.length ?? 0;
      return node.operation === "remove"
        ? { title: "Remove the profile", detail: "In an external system" }
        : { title: "Update the profile", detail: `Sends ${count} trait${count === 1 ? "" : "s"}` };
    }
    case "action.send":
      return { title: "Send an email", detail: node.topic === undefined ? "No topic" : "Under a topic" };
    case "action.update_trait": {
      const key = node.key === "" ? "a trait" : node.key;
      if (node.change.op === "set") return { title: `${key} = ${valueText(node.change.value)}`, detail: "Set" };
      if (node.change.op === "increment") return { title: `${key} + ${node.change.by}`, detail: "Increase" };
      return { title: key, detail: "Clear the flow's change" };
    }
    case "action.list":
      return { title: listName(node.list_id, names), detail: node.change === "add" ? "Add to the list" : "Remove from the list" };
    case "goal":
      return { title: "Goal reached", detail: node.exit ? "Records a conversion and exits" : "Records a conversion" };
    case "exit":
      return { title: "Leaves the flow", detail: "Ends here" };
  }
}

/** The trigger, in the words of its node. */
export function triggerWords(settings: Settings, names: Names = NO_NAMES): Words {
  const trigger = settings.trigger;
  switch (trigger.type) {
    case "segment":
      return {
        title: `${trigger.on === "entered" ? "Enters" : "Leaves"} ${segmentName(trigger.segment_id, names)}`,
        detail: trigger.on === "entered" ? "Segment entry" : "Segment exit",
      };
    case "event":
      return { title: trigger.event === "" ? "Choose an event" : `Event ${trigger.event}`, detail: "Event" };
    case "schedule":
      return { title: trigger.cron === "" ? "Choose a schedule" : trigger.cron, detail: `Schedule, ${trigger.timezone}` };
    case "manual":
      return { title: "Started by hand", detail: "Manual" };
    case "api":
      return { title: "Started by the API", detail: "API" };
  }
}
