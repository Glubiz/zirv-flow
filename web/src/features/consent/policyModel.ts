import type { components, DefinitionIssue } from "../../api";
import type { ConditionNode } from "../segments";

export type RuleBody = components["schemas"]["ConsentPolicyRuleBody"];
export type RuleRequest = components["schemas"]["ConsentPolicyRuleRequest"];

/** The id of the built-in rule: it matches when the recorded consent does not allow the send. */
export const RECORDED_CONSENT = "recorded_consent";

export const MAX_RULES = 50;
export const MAX_RULE_NAME = 100;

/** One rule being edited. A rule that was never saved has no id; the server gives it one. */
export interface DraftRule {
  /** Stable while the page is open, so a card keeps its place and focus when rules move. */
  key: string;
  id: string | null;
  name: string;
  builtin: boolean;
  condition: ConditionNode | null;
}

let counter = 0;

export function nextKey(): string {
  counter += 1;
  return `rule-${counter}`;
}

/** The tree of a saved condition, without the version beside its keys. */
function treeOf(condition: unknown): ConditionNode | null {
  if (typeof condition !== "object" || condition === null) return null;
  const copy: Record<string, unknown> = { ...condition };
  delete copy["v"];
  return copy as unknown as ConditionNode;
}

/** The draft rules of a saved policy; `keys` carries the keys of the rules already on the page, by position. */
export function rulesFromApi(rules: readonly RuleBody[], keys: readonly string[] = []): DraftRule[] {
  return rules.map((rule, at) => ({
    key: keys[at] ?? nextKey(),
    id: rule.id,
    name: rule.name,
    builtin: rule.builtin,
    condition: rule.builtin ? null : treeOf(rule.condition),
  }));
}

/** The rules as the draft endpoint takes them: a new rule has no id, the built-in rule no condition. */
export function rulesToRequest(rules: readonly DraftRule[]): RuleRequest[] {
  return rules.map((rule) => {
    const request: RuleRequest = { name: rule.name };
    if (rule.id !== null) request.id = rule.id;
    if (!rule.builtin && rule.condition !== null) {
      request.condition = { v: 1, ...rule.condition } as unknown as Record<string, never>;
    }
    return request;
  });
}

/** A rule for the "Add a rule" button. Its condition is empty, so the server refuses it until it is filled in. */
export function newRule(): DraftRule {
  return { key: nextKey(), id: null, name: "New rule", builtin: false, condition: { trait: "", op: "eq" } };
}

/** The rules with the one at `index` moved by `by` places, or the same list when it cannot move. */
export function moveRule<T>(rules: readonly T[], index: number, by: -1 | 1): T[] {
  const target = index + by;
  const moved = rules[index];
  if (moved === undefined || target < 0 || target >= rules.length) return [...rules];
  const next = [...rules];
  next.splice(index, 1);
  next.splice(target, 0, moved);
  return next;
}

/** A string that changes whenever the saved meaning of the draft does, to tell unsaved edits from saved ones. */
export function fingerprint(rules: readonly DraftRule[]): string {
  return JSON.stringify(rulesToRequest(rules));
}

export interface RuleIssues {
  /** Problems with the list as a whole, such as a missing built-in rule. */
  general: string[];
  /** Problems with the rule itself, by position: its name, for one. */
  rule: Map<number, string[]>;
  /** Problems with a rule's condition, by position, with the pointer made relative to the condition. */
  condition: Map<number, DefinitionIssue[]>;
}

/** Sorts the issues of a refused save or publish onto the rules they name. */
export function sortIssues(issues: readonly DefinitionIssue[]): RuleIssues {
  const sorted: RuleIssues = { general: [], rule: new Map(), condition: new Map() };
  for (const issue of issues) {
    const match = /^\/rules(?:\/(\d+)(\/condition(.*))?)?$/.exec(issue.pointer);
    if (match === null) {
      sorted.general.push(issue.code);
      continue;
    }
    const [, index, conditionPart, rest] = match;
    if (index === undefined) {
      sorted.general.push(issue.code);
    } else if (conditionPart === undefined) {
      sorted.rule.set(Number(index), [...(sorted.rule.get(Number(index)) ?? []), issue.code]);
    } else {
      sorted.condition.set(Number(index), [...(sorted.condition.get(Number(index)) ?? []), { pointer: rest ?? "", code: issue.code }]);
    }
  }
  return sorted;
}

const RULE_ISSUES: Record<string, string> = {
  too_many_rules: `A policy has at most ${MAX_RULES} rules.`,
  builtin_rule_required: "The built-in recorded consent rule has to stay in the policy. It can move, but not be removed.",
  invalid_rule_id: "This rule has an id that is not valid. Remove it and add it again.",
  duplicate_rule_id: "Two rules have the same id.",
  invalid_rule_name: `Give the rule a name of 1 to ${MAX_RULE_NAME} characters.`,
  builtin_rule_has_condition: "The built-in rule has no condition of its own.",
  rule_condition_required: "This rule needs a condition.",
};

export function ruleIssueMessage(code: string): string {
  return RULE_ISSUES[code] ?? `This rule is not valid (${code}).`;
}
