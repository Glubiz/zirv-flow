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

/**
 * The text form of a value map is one `from=to` pair per line. A backslash writes what a line cannot
 * hold: `\\` is a backslash, `\=` an equals sign (needed only in a `from`, which ends at the first
 * unescaped `=`), `\n` and `\r` a line break, `\t` a tab, `\s` a space, and `\uXXXX` any other
 * whitespace character. Spaces, tabs and the like are trimmed from the ends of each side, so at an
 * end they are always written as an escape. A backslash followed by anything else stands for itself,
 * so one is written `\\` only where it would otherwise start an escape.
 */
const ESCAPES: Record<string, string> = { "\\": "\\", "=": "=", n: "\n", r: "\r", t: "\t", s: " " };

function unescapeText(text: string): string {
  let out = "";
  for (let i = 0; i < text.length; i += 1) {
    const char = text.charAt(i);
    if (char !== "\\" || i === text.length - 1) {
      out += char;
      continue;
    }
    const next = text.charAt(i + 1);
    const hex = next === "u" ? text.slice(i + 2, i + 6) : "";
    if (/^[0-9a-fA-F]{4}$/.test(hex)) {
      out += String.fromCharCode(parseInt(hex, 16));
      i += 5;
    } else if (ESCAPES[next] !== undefined) {
      out += ESCAPES[next] ?? "";
      i += 1;
    } else {
      out += char;
    }
  }
  return out;
}

/** Whether a backslash written before this text would start an escape. */
function startsEscape(rest: string): boolean {
  const next = rest.charAt(0);
  return ESCAPES[next] !== undefined || (next === "u" && /^[0-9a-fA-F]{4}$/.test(rest.slice(1, 5)));
}

function escapeText(text: string, key: boolean): string {
  const edge = (char: string, index: number) => /\s/.test(char) && (/^\s*$/.test(text.slice(0, index + 1)) || /^\s*$/.test(text.slice(index)));
  const pieces: string[] = [];
  for (let i = 0; i < text.length; i += 1) {
    const char = text.charAt(i);
    if (char === "=" && key) pieces.push("\\=");
    else if (char === "\n") pieces.push("\\n");
    else if (char === "\r") pieces.push("\\r");
    else if (edge(char, i)) pieces.push(char === " " ? "\\s" : char === "\t" ? "\\t" : `\\u${char.charCodeAt(0).toString(16).padStart(4, "0")}`);
    else pieces.push(char);
  }
  // A backslash is doubled only where what follows it would read as an escape, so `C:\dir` shows as
  // stored. Right to left, so each one sees the text written after it; a key is followed by its `=`.
  let rest = key ? "=" : "";
  for (let i = pieces.length - 1; i >= 0; i -= 1) {
    const piece = pieces[i] ?? "";
    rest = (piece === "\\" && startsEscape(rest) ? "\\\\" : piece) + rest;
  }
  return key ? rest.slice(0, -1) : rest;
}

/** The index of the first `=` that no backslash escapes, or -1. */
function splitAt(line: string): number {
  for (let i = 0; i < line.length; i += 1) {
    if (line[i] === "\\") i += 1;
    else if (line[i] === "=") return i;
  }
  return -1;
}

export function parseEntries(text: string): Record<string, string> {
  const pairs: [string, string][] = [];
  for (const line of text.split(/\r?\n/)) {
    const at = splitAt(line);
    if (at < 0) continue;
    pairs.push([unescapeText(line.slice(0, at).trim()), unescapeText(line.slice(at + 1).trim())]);
  }
  // From entries, so a key such as `__proto__` stays an own property.
  return Object.fromEntries(pairs);
}

function formatEntries(map: Record<string, string>): string {
  return Object.entries(map)
    .map(([from, to]) => `${escapeText(from, true)}=${escapeText(to, false)}`)
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
