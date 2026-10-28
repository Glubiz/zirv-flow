import { json, MEMBER, OWNER, session, type Call, type Handler } from "./mockApi";
import { catalogRoutes, members, WS } from "./segmentsApi";

/** Fixtures and routes shared by the theme tests. */

export { WS };

export const THEME_ID = "77777777-0000-4000-8000-000000000001";
const BASE = `${WS}/themes/${THEME_ID}`;

export type Doc = Record<string, unknown>;

export function tokens(overrides: Doc = {}) {
  return {
    primary: "#7a3e1d",
    text: "#1f1a17",
    background: "#fbf7f2",
    link: "#7a3e1d",
    button: "#b06a35",
    button_text: "#ffffff",
    content_width: 600,
    font: "georgia",
    web_font_url: null,
    button_radius: 6,
    link_underline: true,
    logo: null,
    footer_snippet: null,
    ...overrides,
  };
}

export function themeBody(overrides: Doc = {}) {
  return {
    id: THEME_ID,
    name: "Warm",
    current_version: null,
    archived_at: null,
    is_default: false,
    created_at: "2026-10-01T09:00:00Z",
    updated_at: "2026-10-10T09:00:00Z",
    created_by: null,
    ...overrides,
  };
}

export function listItem(overrides: Doc = {}, used = 0, theTokens: Doc = tokens()) {
  return { ...themeBody(overrides), tokens: theTokens, used_by_templates: used };
}

export function themeDetail(overrides: Doc = {}, theTokens: Doc = tokens(), revision = 1) {
  return {
    ...themeBody(overrides),
    draft: { tokens: theTokens, revision, updated_at: "2026-10-10T09:00:00Z", updated_by: null },
  };
}

export function previewBody(overrides: Doc = {}) {
  return { source: "draft", version: null, rendered: true, html: "<p>A heading</p>", text: "A heading", issues: [], ...overrides };
}

export const contrastIssue = (at: string, ratio = "4.20") => ({
  code: "low_contrast",
  severity: "warning",
  at,
  message: `contrast ${ratio}:1, WCAG AA asks 4.5:1`,
});

export function usedBy(overrides: Doc = {}) {
  return { named_templates: [], default_templates: [], active_flows: [], ...overrides };
}

export function templateUse(n: number) {
  return { id: `t${n}`, name: `Template ${n}`, in_draft: true, in_current: false };
}

interface Options {
  /** `admin` publishes and archives; `member` edits; `reader` only looks. */
  role?: "admin" | "editor" | "reader";
  detail?: Record<string, unknown>;
  /** The list the themes page reads. */
  list?: unknown[];
  preview?: (tokens: Doc, asked: number) => Record<string, unknown> | Response;
  uses?: Record<string, unknown>;
  versions?: unknown[];
  /** Answers a draft save; the default accepts it and moves the revision on. */
  save?: (body: { tokens: Doc; revision?: number | null }, revision: number) => Response | null;
}

/** The session, the catalog, the theme and its draft, preview, publish, versions and where-used endpoints. */
export function themeRoutes({ role = "admin", detail, list, preview, uses = usedBy(), versions = [], save }: Options = {}): Record<string, Handler> {
  let current: Record<string, unknown> = detail ?? themeDetail();
  let asked = 0;
  const actorRoutes: Record<string, Handler> =
    role === "admin"
      ? { "GET /api/v1/session": () => json(session(OWNER)), "GET /api/v1/workspaces/ws1/members": () => json(members("admin")) }
      : {
          "GET /api/v1/session": () => json(session(MEMBER)),
          "GET /api/v1/workspaces/ws1/members": () => json(members(role === "editor" ? "editor" : null)),
        };
  return {
    ...actorRoutes,
    ...catalogRoutes(),
    [`GET ${WS}/themes`]: () => json(list ?? [{ ...current, tokens: (current["draft"] as { tokens: Doc }).tokens, used_by_templates: 0 }]),
    [`GET ${BASE}`]: () => json(current),
    [`PUT ${BASE}/draft`]: async (request) => {
      const body = (await request.json()) as { tokens: Doc; revision?: number | null };
      const draft = current["draft"] as { revision: number };
      const refused = save?.(body, draft.revision);
      if (refused) return refused;
      current = { ...current, draft: { ...draft, tokens: body.tokens, revision: draft.revision + 1 } };
      return json(current);
    },
    [`POST ${BASE}/preview`]: () => {
      asked += 1;
      const answer = preview ? preview((current["draft"] as { tokens: Doc }).tokens, asked) : previewBody();
      return answer instanceof Response ? answer : json(answer);
    },
    [`POST ${BASE}/publish`]: () => {
      const next = ((current["current_version"] as number | null) ?? 0) + 1;
      current = { ...current, current_version: next };
      return json({
        theme: current,
        version: { version: next, tokens: (current["draft"] as { tokens: Doc }).tokens, published_at: "2026-10-11T09:00:00Z", warnings: [] },
        used_by: uses,
      });
    },
    [`GET ${BASE}/versions`]: () => json(versions),
    [`GET ${BASE}/where-used`]: () => json(uses),
    [`GET ${WS}/snippets`]: () => json([]),
    [`GET ${WS}/assets`]: () => json({ assets: [], next_cursor: null, used_bytes: 0, limit_bytes: 1000, max_file_bytes: 1000 }),
  };
}

/** The draft saves, oldest first. */
export function saves(calls: Call[]): { tokens: Doc; revision?: number | null }[] {
  return calls.filter((call) => call.method === "PUT" && call.path.endsWith("/draft")).map((call) => call.body as { tokens: Doc; revision?: number | null });
}

/** The preview requests, oldest first. */
export function previews(calls: Call[]): Call[] {
  return calls.filter((call) => call.method === "POST" && call.path.endsWith("/preview"));
}
