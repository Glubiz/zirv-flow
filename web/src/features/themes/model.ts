import type { components } from "../../api";

export type Theme = components["schemas"]["ThemeBody"];
export type ThemeListItem = components["schemas"]["ThemeListItemBody"];
export type ThemeDetail = components["schemas"]["ThemeDetailBody"];
export type ThemeUsedBy = components["schemas"]["ThemeUsedByBody"];
export type ThemeVersion = components["schemas"]["ThemeVersionBody"];
export type ThemePreview = components["schemas"]["ThemePreviewBody"];
export type ThemeIssue = components["schemas"]["TemplateIssueProblem"];

/** The tokens of a theme as the editor holds them. `web_font_url` has no field and travels as stored. */
export interface ThemeTokens {
  primary: string;
  text: string;
  background: string;
  link: string;
  button: string;
  button_text: string;
  content_width: number;
  font: string;
  web_font_url?: string | null;
  button_radius: number;
  link_underline: boolean;
  logo?: string | null;
  footer_snippet?: string | null;
}

export type ColourKey = "primary" | "background" | "text" | "link" | "button" | "button_text";

/** The colour fields in the order of the form. */
export const COLOURS: readonly { key: ColourKey; label: string }[] = [
  { key: "primary", label: "Primary" },
  { key: "background", label: "Background" },
  { key: "text", label: "Text" },
  { key: "link", label: "Link" },
  { key: "button", label: "Button" },
  { key: "button_text", label: "Button text" },
];

const DEFAULTS: ThemeTokens = {
  primary: "#1a4fd6",
  text: "#222222",
  background: "#ffffff",
  link: "#1a4fd6",
  button: "#1a4fd6",
  button_text: "#ffffff",
  content_width: 600,
  font: "arial",
  button_radius: 0,
  link_underline: true,
};

/** The stored tokens, with a default for any the server left out. */
export function readTokens(stored: object): ThemeTokens {
  return { ...DEFAULTS, ...(stored as Partial<ThemeTokens>) };
}

/** The generated client types tokens as an object with no keys; this is the one place that says otherwise. */
export function asTokens(tokens: ThemeTokens): Record<string, never> {
  return tokens as unknown as Record<string, never>;
}

const HEX = /^#(?:[0-9a-f]{3}|[0-9a-f]{6})$/i;

export function isHex(text: string): boolean {
  return HEX.test(text);
}

export const MIN_WIDTH = 480;
export const MAX_WIDTH = 800;

export type Corners = "square" | "rounded" | "pill";

export const CORNERS: readonly { id: Corners; label: string; radius: number }[] = [
  { id: "square", label: "Square", radius: 0 },
  { id: "rounded", label: "Rounded", radius: 6 },
  { id: "pill", label: "Pill", radius: 40 },
];

/** The corner style a stored radius is; none when it matches no style, until the person picks one. */
export function cornersOf(radius: number): Corners | null {
  return CORNERS.find((corners) => corners.radius === radius)?.id ?? null;
}

/** The CSS of each font stack, as the renderer spells them. */
export const FONT_CSS: Record<string, string> = {
  arial: "Arial, Helvetica, sans-serif",
  helvetica: "Helvetica, Arial, sans-serif",
  verdana: "Verdana, Geneva, sans-serif",
  tahoma: "Tahoma, Geneva, sans-serif",
  trebuchet: "'Trebuchet MS', Helvetica, sans-serif",
  georgia: "Georgia, 'Times New Roman', serif",
  times: "'Times New Roman', Times, serif",
  courier: "'Courier New', Courier, monospace",
};

export function fontCss(font: string): string {
  return FONT_CSS[font] ?? FONT_CSS["arial"] ?? "sans-serif";
}

/** The colour pair each contrast warning concerns, and the ratio it asks for. */
const PAIRS: Record<string, { text: string; asks: string }> = {
  "/theme/primary": { text: "Primary on Background", asks: "3" },
  "/theme/text": { text: "Text on Background", asks: "4.5" },
  "/theme/link": { text: "Link on Background", asks: "4.5" },
  "/theme/button_text": { text: "Button text on Button", asks: "4.5" },
};

/** The contrast warnings shown under each colour field: the last cell of the row of their pair. */
export const WARNING_AFTER: Record<ColourKey, readonly string[]> = {
  primary: [],
  background: ["/theme/primary"],
  text: [],
  link: ["/theme/text", "/theme/link"],
  button: [],
  button_text: ["/theme/button_text"],
};

export function isContrast(issue: ThemeIssue): boolean {
  return issue.code === "low_contrast" && issue.at in PAIRS;
}

/** "Button text on Button is 4.2:1. Aim for 4.5:1." from the server's pointer and message. */
export function contrastNote(issue: ThemeIssue): string {
  const pair = PAIRS[issue.at];
  const ratio = /contrast ([\d.]+):1/.exec(issue.message ?? "")?.[1];
  if (!pair || !ratio) return "These two colours are hard to tell apart.";
  return `${pair.text} is ${Number(ratio)}:1. Aim for ${pair.asks}:1.`;
}

export function isArchived(theme: Theme): boolean {
  return theme.archived_at !== null && theme.archived_at !== undefined;
}

export function isPublished(theme: Theme): boolean {
  return theme.current_version !== null && theme.current_version !== undefined;
}

/** The line under a theme's name: always words. */
export function statusText(theme: Theme): string {
  if (isArchived(theme)) return "Archived";
  return isPublished(theme) ? `Draft · v${theme.current_version} live` : "Not published";
}

/** The version a publish would make next. */
export function nextVersion(theme: Theme): number {
  return (theme.current_version ?? 0) + 1;
}

export type UserKind = "named" | "default" | "flow";

export const USER_LABEL: Record<UserKind, string> = { named: "Names this theme", default: "Follows the default", flow: "Active flow" };

export interface ThemeUser {
  kind: UserKind;
  id: string;
  name: string;
  href: string;
}

/** The templates that name a theme, those that follow the default, then the active flows. */
export function usersOf(workspaceId: string, used: ThemeUsedBy): ThemeUser[] {
  const base = `/w/${workspaceId}`;
  return [
    ...used.named_templates.map((template) => ({ kind: "named" as const, id: template.id, name: template.name, href: `${base}/templates/${template.id}` })),
    ...used.default_templates.map((template) => ({ kind: "default" as const, id: template.id, name: template.name, href: `${base}/templates/${template.id}` })),
    ...used.active_flows.map((flow) => ({ kind: "flow" as const, id: flow.flow_id, name: flow.flow_name, href: `${base}/flows/${flow.flow_id}` })),
  ];
}

/** The templates a publish reaches: those that name the theme and, for the default, those that follow it. */
export function reachedTemplates(used: ThemeUsedBy): { id: string; name: string }[] {
  const seen = new Map<string, string>();
  for (const template of [...used.named_templates, ...used.default_templates]) seen.set(template.id, template.name);
  return [...seen].map(([id, name]) => ({ id, name }));
}
