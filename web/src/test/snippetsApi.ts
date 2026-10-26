import { json, MEMBER, OWNER, session, type Call, type Handler } from "./mockApi";
import { catalogRoutes, members, WS } from "./segmentsApi";

/** Fixtures and routes shared by the snippet tests. */

export { WS };

export const SNIPPET_ID = "66666666-0000-4000-8000-000000000001";
const BASE = `${WS}/snippets/${SNIPPET_ID}`;

export type Doc = Record<string, unknown>;

export function snippetBody(overrides: Doc = {}) {
  return {
    id: SNIPPET_ID,
    name: "Shipping promise",
    current_version: null,
    archived_at: null,
    created_at: "2026-10-01T09:00:00Z",
    updated_at: "2026-10-10T09:00:00Z",
    created_by: null,
    ...overrides,
  };
}

export function snippetDetail(overrides: Doc = {}, blocks: unknown[] = [], revision = 1) {
  return {
    ...snippetBody(overrides),
    draft: { content: { blocks }, revision, updated_at: "2026-10-10T09:00:00Z", updated_by: null },
  };
}

export function previewBody(overrides: Doc = {}) {
  return { source: "draft", version: null, rendered: true, html: "<p>Free returns</p>", text: "Free returns", issues: [], ...overrides };
}

export function usedBy(overrides: Doc = {}) {
  return { templates: [], themes: [], active_flows: [], ...overrides };
}

export function templateUse(n: number) {
  return { id: `t${n}`, name: `Template ${n}`, in_draft: true, in_current: n % 2 === 0 };
}

export const heading = (id: string, text: string) => ({ type: "heading", id, level: 2, text });

interface Options {
  /** `admin` publishes and archives; `member` edits; `reader` only looks. */
  role?: "admin" | "member" | "reader";
  detail?: Record<string, unknown>;
  preview?: (content: Doc, asked: number) => Record<string, unknown> | Response;
  uses?: Record<string, unknown>;
  versions?: unknown[];
  /** Answers a draft save; the default accepts it and moves the revision on. */
  save?: (body: { content: Doc; revision?: number | null }, revision: number) => Response | null;
}

/** The session, the catalog, the snippet and its draft, preview, publish, versions and where-used endpoints. */
export function snippetRoutes({ role = "admin", detail, preview, uses = usedBy(), versions = [], save }: Options = {}): Record<string, Handler> {
  let current: Record<string, unknown> = detail ?? snippetDetail();
  let asked = 0;
  const actorRoutes: Record<string, Handler> =
    role === "admin"
      ? { "GET /api/v1/session": () => json(session(OWNER)), "GET /api/v1/workspaces/ws1/members": () => json(members("admin")) }
      : {
          "GET /api/v1/session": () => json(session(MEMBER)),
          "GET /api/v1/workspaces/ws1/members": () => json(members(role === "member" ? "member" : null)),
        };
  return {
    ...actorRoutes,
    ...catalogRoutes(),
    [`GET ${WS}/snippets`]: () => json([current]),
    [`GET ${BASE}`]: () => json(current),
    [`PUT ${BASE}/draft`]: async (request) => {
      const body = (await request.json()) as { content: Doc; revision?: number | null };
      const draft = current["draft"] as { revision: number };
      const refused = save?.(body, draft.revision);
      if (refused) return refused;
      current = { ...current, draft: { ...draft, content: body.content, revision: draft.revision + 1 } };
      return json(current);
    },
    [`POST ${BASE}/preview`]: () => {
      asked += 1;
      const content = (current["draft"] as { content: Doc }).content;
      const answer = preview ? preview(content, asked) : previewBody();
      return answer instanceof Response ? answer : json(answer);
    },
    [`POST ${BASE}/publish`]: () => {
      const next = ((current["current_version"] as number | null) ?? 0) + 1;
      current = { ...current, current_version: next };
      return json({
        snippet: current,
        version: { version: next, content: (current["draft"] as { content: Doc }).content, published_at: "2026-10-11T09:00:00Z", warnings: [] },
        used_by: uses,
      });
    },
    [`GET ${BASE}/versions`]: () => json(versions),
    [`GET ${BASE}/where-used`]: () => json(uses),
    [`GET ${WS}/assets`]: () => json({ assets: [], next_before: null, used_bytes: 0, limit_bytes: 1000, max_file_bytes: 1000 }),
  };
}

/** The draft saves, oldest first. */
export function saves(calls: Call[]): { content: Doc; revision?: number | null }[] {
  return calls.filter((call) => call.method === "PUT" && call.path.endsWith("/draft")).map((call) => call.body as { content: Doc; revision?: number | null });
}
