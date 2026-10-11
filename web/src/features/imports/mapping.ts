import type { components } from "../../api";

export type Mapping = components["schemas"]["MappingBody"];
type Target = components["schemas"]["TargetBody"];
type Transform = components["schemas"]["TransformBody"];
export type TraitType = components["schemas"]["TraitTypeBody"];

export const TRAIT_TYPES: readonly TraitType[] = ["string", "number", "boolean", "timestamp", "date", "string_list"];

export type IdentifierChoice = "email" | "phone" | "anonymous_id" | "external";

/** What a column becomes, as the editor holds it while it is being filled in. */
export type TargetChoice =
  | { kind: "ignore" }
  | { kind: "identifier"; identifier: IdentifierChoice; namespace: string }
  | { kind: "trait"; key: string }
  | { kind: "list"; listId: string };

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

export interface ColumnDraft {
  column: string;
  target: TargetChoice;
  transforms: TransformDraft[];
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

function transformFromApi(transform: Transform): TransformDraft {
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

export function identifierKind(choice: Extract<TargetChoice, { kind: "identifier" }>): string | null {
  if (choice.identifier !== "external") return choice.identifier;
  const namespace = choice.namespace.trim();
  return namespace === "" ? null : `external:${namespace}`;
}

/** The API target for a choice, or null while the choice is incomplete. */
export function targetToApi(choice: TargetChoice): Target | null {
  switch (choice.kind) {
    case "ignore":
      return { type: "ignore" };
    case "identifier": {
      const kind = identifierKind(choice);
      return kind === null ? null : { type: "identifier", kind };
    }
    case "trait":
      return choice.key === "" ? null : { type: "trait", key: choice.key };
    case "list":
      return choice.listId === "" ? null : { type: "list", list_id: choice.listId };
  }
}

function targetFromApi(target: Target): TargetChoice {
  switch (target.type) {
    case "ignore":
      return { kind: "ignore" };
    case "trait":
      return { kind: "trait", key: target.key };
    case "list":
      return { kind: "list", listId: target.list_id };
    case "identifier":
      if (target.kind.startsWith("external:")) {
        return { kind: "identifier", identifier: "external", namespace: target.kind.slice("external:".length) };
      }
      if (target.kind === "email" || target.kind === "phone" || target.kind === "anonymous_id") {
        return { kind: "identifier", identifier: target.kind, namespace: "" };
      }
      return { kind: "ignore" };
  }
}

/** A guess for a file with no mapping yet: email and phone columns by their names. */
export function guessDrafts(header: readonly string[]): ColumnDraft[] {
  let taken = false;
  return header.map((column) => {
    const name = column.toLowerCase();
    let target: TargetChoice = { kind: "ignore" };
    let transforms: TransformDraft[] = [];
    if (!taken && /e-?mail/.test(name)) {
      target = { kind: "identifier", identifier: "email", namespace: "" };
      transforms = [newTransform("trim"), newTransform("lower")];
      taken = true;
    } else if (/(phone|mobile|tel)/.test(name)) {
      target = { kind: "identifier", identifier: "phone", namespace: "" };
      transforms = [newTransform("trim")];
    }
    return { column, target, transforms };
  });
}

export interface DraftSet {
  drafts: ColumnDraft[];
  keyColumn: string;
  /** Columns the saved mapping has more than one rule for; the editor shows only the first. */
  repeated: string[];
}

/** The editor's state for a saved mapping, or a guess when there is none. */
export function mappingToDrafts(header: readonly string[], mapping: Mapping | null | undefined): DraftSet {
  if (!mapping) {
    return { drafts: guessDrafts(header), keyColumn: "", repeated: [] };
  }
  const repeated: string[] = [];
  const drafts = header.map((column): ColumnDraft => {
    const rules = mapping.columns.filter((rule) => rule.column === column);
    if (rules.length > 1) repeated.push(column);
    const rule = rules[0];
    if (!rule) return { column, target: { kind: "ignore" }, transforms: [] };
    return {
      column,
      target: targetFromApi(rule.target),
      transforms: (rule.transforms ?? []).map(transformFromApi),
    };
  });
  return { drafts, keyColumn: mapping.key_column ?? "", repeated };
}

/** The mapping to save: every column that is not ignored, in file order. */
export function draftsToMapping(drafts: readonly ColumnDraft[], keyColumn: string): Mapping {
  const columns: Mapping["columns"] = [];
  for (const draft of drafts) {
    if (draft.target.kind === "ignore") continue;
    const target = targetToApi(draft.target);
    if (target === null) continue;
    const transforms = draft.transforms.flatMap((step) => {
      const api = transformToApi(step);
      return api === null ? [] : [api];
    });
    columns.push({ column: draft.column, target, transforms });
  }
  return { key_column: keyColumn === "" ? null : keyColumn, columns };
}

/** What stops the mapping from being saved, in words, before the server is asked. */
export function draftProblems(drafts: readonly ColumnDraft[]): string[] {
  const problems: string[] = [];
  const identifiers = drafts.filter((draft) => draft.target.kind === "identifier");
  if (identifiers.length === 0) {
    problems.push("Choose at least one column that identifies a person, such as an email address.");
  }
  for (const draft of drafts) {
    if (draft.target.kind !== "ignore" && targetToApi(draft.target) === null) {
      problems.push(`Finish the choice for ${draft.column}.`);
    }
    for (const step of draft.transforms) {
      if (transformToApi(step) === null) {
        problems.push(`Finish the "${opLabel(step.op)}" step for ${draft.column}.`);
      }
    }
  }
  const traitKeys = drafts.flatMap((draft) => (draft.target.kind === "trait" ? [draft.target.key] : []));
  if (new Set(traitKeys).size !== traitKeys.length) {
    problems.push("Two columns fill the same field; choose one of them.");
  }
  return problems;
}
