import type { components } from "../../api";

export type UtmSettings = components["schemas"]["UtmSettingsBody"];

/** The placeholders a pattern may use; the server refuses any other. */
export const UTM_PLACEHOLDERS = ["workspace", "flow", "flow_version", "step", "template", "template_version", "category"] as const;

const MAX_PATTERN_BYTES = 200;
const MAX_DOMAINS = 50;

export type UtmField = "source" | "medium" | "campaign" | "content";

export const UTM_FIELDS: readonly { id: UtmField; param: string; label: string }[] = [
  { id: "source", param: "utm_source", label: "Source pattern" },
  { id: "medium", param: "utm_medium", label: "Medium pattern" },
  { id: "campaign", param: "utm_campaign", label: "Campaign pattern" },
  { id: "content", param: "utm_content", label: "Content pattern" },
];

function hasControlCharacter(text: string): boolean {
  for (const character of text) {
    const code = character.codePointAt(0) ?? 0;
    if (code < 0x20 || code === 0x7f) return true;
  }
  return false;
}

/** What is wrong with a pattern, as the server would say it; undefined when it is fine. */
export function checkPattern(pattern: string): string | undefined {
  if (new TextEncoder().encode(pattern).length > MAX_PATTERN_BYTES) return `Use at most ${MAX_PATTERN_BYTES} bytes.`;
  if (hasControlCharacter(pattern)) return "Remove the control characters.";
  let rest = pattern;
  for (;;) {
    const at = rest.search(/[{}]/);
    if (at < 0) return undefined;
    if (rest[at] === "}") return "A closing brace has no opening brace.";
    const after = rest.slice(at + 1);
    const close = after.indexOf("}");
    if (close < 0) return "An opening brace is not closed.";
    const name = after.slice(0, close);
    if (!(UTM_PLACEHOLDERS as readonly string[]).includes(name)) {
      return `Unknown placeholder {${name}}. Use ${UTM_PLACEHOLDERS.map((entry) => `{${entry}}`).join(", ")}.`;
    }
    rest = after.slice(close + 1);
  }
}

/** Lowercase, with runs of other characters turned into one hyphen. */
function slug(text: string): string {
  return text
    .toLowerCase()
    .replace(/[^\p{L}\p{N}]+/gu, "-")
    .replace(/^-+|-+$/g, "");
}

/** The values of a live example: what a send from a flow would put in. */
interface ExampleValues {
  workspace: string;
  flow: string;
  flow_version: string;
  step: string;
  template: string;
  template_version: string;
  category: string;
}

const EXAMPLE_VALUES: Omit<ExampleValues, "workspace"> = {
  flow: "welcome-series",
  flow_version: "3",
  step: "send-welcome",
  template: "welcome-email",
  template_version: "2",
  category: "marketing",
};

/** Expands a valid pattern: placeholder values slugged, the whole result URL-encoded. */
function expandPattern(pattern: string, values: ExampleValues): string {
  const text = pattern.replace(/\{([^{}]*)\}/g, (_, name: string) => slug(values[name as keyof ExampleValues]));
  return encodeURIComponent(text).replace(/[!'()*]/g, (c) => `%${c.charCodeAt(0).toString(16).toUpperCase()}`);
}

/** One domain per line or separated by commas or spaces; lower-case, without a leading `*.` or dot, no duplicates. */
export function parseDomains(text: string): string[] {
  const seen = new Set<string>();
  for (const part of text.split(/[\s,;]+/)) {
    const domain = part.trim().toLowerCase().replace(/^\*\./, "").replace(/^\./, "").replace(/\.$/, "");
    if (domain !== "") seen.add(domain);
  }
  return [...seen];
}

const LABEL = "[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?";
const DOMAIN = new RegExp(`^${LABEL}(\\.${LABEL})+$`);

function badDomains(domains: readonly string[]): string[] {
  return domains.filter((domain) => !DOMAIN.test(domain));
}

export interface UtmProblems {
  fields: Partial<Record<UtmField, string>>;
  domains?: string;
}

export function checkUtm(draft: UtmSettings): UtmProblems {
  const problems: UtmProblems = { fields: {} };
  for (const { id } of UTM_FIELDS) {
    const found = checkPattern(draft[id]);
    if (found !== undefined) problems.fields[id] = found;
  }
  const bad = badDomains(draft.domains);
  if (draft.domains.length > MAX_DOMAINS) {
    problems.domains = `List at most ${MAX_DOMAINS} domains.`;
  } else if (bad.length > 0) {
    problems.domains = `Not a domain name: ${bad.join(", ")}. Write names such as example.com.`;
  } else if (draft.enabled && draft.domains.length === 0) {
    problems.domains = "List at least one domain before turning UTM parameters on.";
  }
  return problems;
}

export function hasUtmProblems(problems: UtmProblems): boolean {
  return problems.domains !== undefined || Object.keys(problems.fields).length > 0;
}

export function sameUtm(a: UtmSettings, b: UtmSettings): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

/**
 * A link as the next send would carry it: the parameters in a fixed order, a parameter whose
 * pattern expands to nothing left out. The link is on the first listed domain (or example.com).
 */
export function exampleLink(draft: UtmSettings, workspaceSlug: string): string {
  const values: ExampleValues = { ...EXAMPLE_VALUES, workspace: workspaceSlug };
  const host = draft.domains[0] ?? "example.com";
  const query = UTM_FIELDS.map(({ id, param }) => [param, checkPattern(draft[id]) === undefined ? expandPattern(draft[id], values) : ""] as const)
    .filter(([, value]) => value !== "")
    .map(([param, value]) => `${param}=${value}`)
    .join("&");
  return `https://${host}/offer${query === "" ? "" : `?${query}`}`;
}
