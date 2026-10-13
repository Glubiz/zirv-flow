import type { components } from "../api";

/** One step between a cell and its target, as the API takes it. */
export type Transform = components["schemas"]["TransformBody"];
export type TraitType = components["schemas"]["TraitTypeBody"];

export const TRAIT_TYPES: readonly TraitType[] = ["string", "number", "boolean", "timestamp", "date", "string_list"];

export type TransformOp = Transform["op"];

export const TRANSFORM_OPS: readonly TransformOp[] = [
  "trim",
  "lower",
  "upper",
  "cast",
  "timestamp",
  "concat",
  "constant",
  "value_map",
  "split",
];

/** One transform step with every parameter any step might need; only the ones its `op` uses count. */
export interface TransformDraft {
  /** Stable while the step is on screen, so editing one does not remount the others. */
  id: number;
  op: TransformOp;
  to: TraitType;
  format: string;
  columns: string[];
  separator: string;
  value: string;
  /** One `from=to` pair per line. */
  entries: string;
}

let nextId = 1;

export function newTransform(op: TransformOp): TransformDraft {
  return {
    id: nextId++,
    op,
    to: "string",
    format: "%Y-%m-%d",
    columns: [],
    separator: op === "split" ? "," : " ",
    value: "",
    entries: "",
  };
}

export function opLabel(op: TransformOp): string {
  switch (op) {
    case "trim":
      return "Trim spaces";
    case "lower":
      return "Lowercase";
    case "upper":
      return "Uppercase";
    case "cast":
      return "Convert to a type";
    case "timestamp":
      return "Read as a date or time";
    case "concat":
      return "Join with other columns";
    case "constant":
      return "Use a fixed value";
    case "value_map":
      return "Replace values";
    case "split":
      return "Split into a list";
  }
}

export function parseEntries(text: string): Record<string, string> {
  const map: Record<string, string> = {};
  for (const line of text.split(/\r?\n/)) {
    const at = line.indexOf("=");
    if (at <= 0) continue;
    map[line.slice(0, at).trim()] = line.slice(at + 1).trim();
  }
  return map;
}

function formatEntries(map: Record<string, string>): string {
  return Object.entries(map)
    .map(([from, to]) => `${from}=${to}`)
    .join("\n");
}

/** The API step for a draft, or null while the step is incomplete. */
export function transformToApi(draft: TransformDraft): Transform | null {
  switch (draft.op) {
    case "trim":
    case "lower":
    case "upper":
      return { op: draft.op };
    case "cast":
      return { op: "cast", to: draft.to };
    case "timestamp":
      return draft.format.trim() === "" ? null : { op: "timestamp", format: draft.format.trim() };
    case "concat":
      return draft.columns.length === 0 ? null : { op: "concat", columns: draft.columns, separator: draft.separator };
    case "constant":
      return { op: "constant", value: draft.value };
    case "value_map": {
      const map = parseEntries(draft.entries);
      return Object.keys(map).length === 0 ? null : { op: "value_map", map };
    }
    case "split":
      return draft.separator === "" ? null : { op: "split", separator: draft.separator };
  }
}

export function transformFromApi(transform: Transform): TransformDraft {
  const draft = newTransform(transform.op);
  switch (transform.op) {
    case "cast":
      draft.to = transform.to;
      break;
    case "timestamp":
      draft.format = transform.format;
      break;
    case "concat":
      draft.columns = transform.columns;
      draft.separator = transform.separator;
      break;
    case "constant":
      draft.value = transform.value;
      break;
    case "value_map":
      draft.entries = formatEntries(transform.map);
      break;
    case "split":
      draft.separator = transform.separator;
      break;
    default:
      break;
  }
  return draft;
}

/** The API steps for a chain of drafts, leaving out the incomplete ones. */
export function chainToApi(steps: readonly TransformDraft[]): Transform[] {
  return steps.flatMap((step) => {
    const api = transformToApi(step);
    return api === null ? [] : [api];
  });
}
