import type { components } from "../../api";
import { chainToApi, newTransform, opLabel, transformFromApi, transformToApi, type TransformDraft } from "../../ui";

export type Mapping = components["schemas"]["MappingBody"];
type Target = components["schemas"]["TargetBody"];
type EventRule = components["schemas"]["EventRuleBody"];

/** The most properties an event may carry, as the server allows. */
export const MAX_EVENT_PROPERTIES = 50;

export type IdentifierChoice = "email" | "phone" | "anonymous_id" | "external";

/** What a field becomes, as the editor holds it while it is being filled in. */
export type TargetChoice =
  | { kind: "identifier"; identifier: IdentifierChoice; namespace: string }
  | { kind: "trait"; key: string }
  | { kind: "list"; listId: string };

/** One rule of the people section: a field (or a dotted path into one), where it goes, and the steps between. */
export interface RuleDraft {
  /** Stable while the row is on screen. */
  id: number;
  column: string;
  target: TargetChoice;
  transforms: TransformDraft[];
}

export interface PropertyDraft {
  id: number;
  property: string;
  column: string;
  transforms: TransformDraft[];
}

export interface EventDraft {
  enabled: boolean;
  name: string;
  timestampColumn: string;
  timestampTransforms: TransformDraft[];
  properties: PropertyDraft[];
}

export interface MappingDraft {
  rules: RuleDraft[];
  event: EventDraft;
}

let nextId = 1;

export function newRule(column = ""): RuleDraft {
  return {
    id: nextId++,
    column,
    target: { kind: "identifier", identifier: "email", namespace: "" },
    transforms: [],
  };
}

export function newProperty(): PropertyDraft {
  return { id: nextId++, property: "", column: "", transforms: [] };
}

export function emptyEvent(): EventDraft {
  return { enabled: false, name: "", timestampColumn: "", timestampTransforms: [], properties: [] };
}

/** A start for a stream with no mapping: one rule for an email-like field, when the stream has one. */
export function startingDraft(fields: readonly string[]): MappingDraft {
  const guess = fields.find((name) => /e-?mail/i.test(name));
  const rule = newRule(guess ?? "");
  rule.transforms = guess ? [newTransform("trim"), newTransform("lower")] : [];
  return { rules: [rule], event: emptyEvent() };
}

export function identifierKind(choice: Extract<TargetChoice, { kind: "identifier" }>): string | null {
  if (choice.identifier !== "external") return choice.identifier;
  const namespace = choice.namespace.trim();
  return namespace === "" ? null : `external:${namespace}`;
}

/** The API target for a choice, or null while the choice is incomplete. */
export function targetToApi(choice: TargetChoice): Target | null {
  switch (choice.kind) {
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

function targetFromApi(target: Target): TargetChoice | null {
  switch (target.type) {
    case "ignore":
      return null;
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
      return null;
  }
}

/** The editor's state for a saved mapping, or a start when there is none. */
export function mappingToDraft(fields: readonly string[], mapping: Mapping | null | undefined): MappingDraft {
  if (!mapping) return startingDraft(fields);
  const rules: RuleDraft[] = [];
  for (const rule of mapping.columns) {
    const target = targetFromApi(rule.target);
    if (target === null) continue;
    rules.push({ id: nextId++, column: rule.column, target, transforms: (rule.transforms ?? []).map(transformFromApi) });
  }
  const event = mapping.event;
  return {
    rules,
    event: event
      ? {
          enabled: true,
          name: event.name,
          timestampColumn: event.timestamp?.column ?? "",
          timestampTransforms: (event.timestamp?.transforms ?? []).map(transformFromApi),
          properties: (event.properties ?? []).map((property) => ({
            id: nextId++,
            property: property.property,
            column: property.column,
            transforms: (property.transforms ?? []).map(transformFromApi),
          })),
        }
      : emptyEvent(),
  };
}

/** The mapping to send: complete rules in order, and the event when it is switched on. */
export function draftToMapping(draft: MappingDraft): Mapping {
  const columns: Mapping["columns"] = [];
  for (const rule of draft.rules) {
    const target = targetToApi(rule.target);
    if (target === null || rule.column.trim() === "") continue;
    columns.push({ column: rule.column.trim(), target, transforms: chainToApi(rule.transforms) });
  }
  const mapping: Mapping = { key_column: null, columns };
  if (draft.event.enabled) {
    const event: EventRule = {
      name: draft.event.name.trim(),
      properties: draft.event.properties
        .filter((property) => property.property.trim() !== "" && property.column.trim() !== "")
        .map((property) => ({
          property: property.property.trim(),
          column: property.column.trim(),
          transforms: chainToApi(property.transforms),
        })),
    };
    if (draft.event.timestampColumn.trim() !== "") {
      event.timestamp = {
        column: draft.event.timestampColumn.trim(),
        transforms: chainToApi(draft.event.timestampTransforms),
      };
    }
    mapping.event = event;
  }
  return mapping;
}

/** What stops the mapping from being previewed or saved, in words, before the server is asked. */
export function draftProblems(draft: MappingDraft, hasPrimaryKey: boolean): string[] {
  const problems: string[] = [];
  if (!draft.rules.some((rule) => rule.target.kind === "identifier")) {
    problems.push("Choose at least one field that identifies a person, such as an email address.");
  }
  for (const rule of draft.rules) {
    const name = rule.column.trim() === "" ? "a row" : rule.column;
    if (rule.column.trim() === "") problems.push("Name the field of every row.");
    else if (targetToApi(rule.target) === null) problems.push(`Finish the choice for ${name}.`);
    for (const step of rule.transforms) {
      if (transformToApi(step) === null) problems.push(`Finish the "${opLabel(step.op)}" step for ${name}.`);
    }
  }
  const traitKeys = draft.rules.flatMap((rule) => (rule.target.kind === "trait" ? [rule.target.key] : []));
  if (new Set(traitKeys).size !== traitKeys.length) {
    problems.push("Two rows fill the same field; choose one of them.");
  }
  if (draft.event.enabled) {
    if (!hasPrimaryKey) problems.push("An event needs a primary key on the stream, so a re-read never repeats it.");
    if (draft.event.name.trim() === "") problems.push("Name the event.");
    if (draft.event.properties.length > MAX_EVENT_PROPERTIES) {
      problems.push(`An event takes at most ${MAX_EVENT_PROPERTIES} properties.`);
    }
    const names = draft.event.properties.map((property) => property.property.trim());
    if (draft.event.properties.some((property) => property.property.trim() === "" || property.column.trim() === "")) {
      problems.push("Give every property a name and a field.");
    }
    if (new Set(names).size !== names.length) problems.push("Two properties have the same name.");
    for (const step of [...draft.event.timestampTransforms, ...draft.event.properties.flatMap((p) => p.transforms)]) {
      if (transformToApi(step) === null) problems.push(`Finish the "${opLabel(step.op)}" step of the event.`);
    }
  }
  return problems;
}
