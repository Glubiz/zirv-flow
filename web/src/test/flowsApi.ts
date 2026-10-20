import { json, MEMBER, session, type Handler } from "./mockApi";
import { actor, catalogRoutes, members, SEGMENT_ID, WS } from "./segmentsApi";

/** Fixtures and routes shared by the flow tests. */

export { WS };

export const FLOW_ID = "55555555-0000-4000-8000-000000000001";

export type Issue = { code: string; node?: string; edge?: string; at?: string };

export function flowBody(overrides: Record<string, unknown> = {}) {
  return {
    id: FLOW_ID,
    name: "Welcome series",
    state: "draft",
    current_version: null,
    created_at: "2026-10-01T09:00:00Z",
    updated_at: "2026-10-10T09:00:00Z",
    archived_at: null,
    created_by: null,
    ...overrides,
  };
}

/**
 * A wait, then a branch on one condition: yes goes to a trait update, no goes to a webhook, and both
 * end at the exit. Every port is wired.
 */
export function sampleGraph() {
  return {
    v: 1,
    nodes: [
      { id: "n1", type: "wait.duration", seconds: 86_400 },
      { id: "n2", type: "branch", conditions: [{ trait: "city", op: "eq", value: "Aarhus" }] },
      { id: "n3", type: "action.update_trait", key: "city", change: { op: "set", value: "Aarhus" } },
      { id: "n4", type: "action.webhook", url: "https://crm.example.com/hooks", trait_keys: ["city"] },
      { id: "n5", type: "exit" },
    ],
    edges: [
      { id: "e1", from: "n1", port: "next", to: "n2" },
      { id: "e2", from: "n2", port: "case_0", to: "n3" },
      { id: "e3", from: "n2", port: "else", to: "n4" },
      { id: "e4", from: "n3", port: "next", to: "n5" },
      { id: "e5", from: "n4", port: "next", to: "n5" },
    ],
  };
}

export function sampleSettings() {
  return { trigger: { type: "segment", segment_id: SEGMENT_ID, on: "entered" }, reentry: { mode: "once" } };
}

export function flowDetail(overrides: Record<string, unknown> = {}) {
  return { ...flowBody(), graph: sampleGraph(), settings: sampleSettings(), ...overrides };
}

interface Options {
  /** `writer` is an admin; `member` may edit but not publish; `reader` may only look. */
  role?: "writer" | "member" | "reader";
  flow?: Record<string, unknown>;
  /** The issues the validate endpoint reports, or a function of how many times it was asked. */
  issues?: Issue[] | ((asked: number) => Issue[]);
}

/** The session, the catalog, the flow and its save and validate endpoints. PATCH applies what it is sent. */
export function flowRoutes({ role = "writer", flow, issues = [] }: Options = {}): Record<string, Handler> {
  let current: Record<string, unknown> = flowDetail(flow);
  let asked = 0;
  return {
    ...(role === "member"
      ? {
          "GET /api/v1/session": () => json(session(MEMBER)),
          "GET /api/v1/workspaces/ws1/members": () => json(members("member")),
        }
      : actor(role)),
    ...catalogRoutes(),
    [`GET ${WS}/flows/${FLOW_ID}`]: () => json(current),
    [`PATCH ${WS}/flows/${FLOW_ID}`]: async (request) => {
      current = { ...current, ...((await request.json()) as Record<string, unknown>) };
      return json(current);
    },
    [`POST ${WS}/flows/${FLOW_ID}/validate`]: () => {
      asked += 1;
      const found = typeof issues === "function" ? issues(asked) : issues;
      return json({ valid: found.length === 0, issues: found });
    },
  };
}

/** What the dry run of a publish reports; by default a valid flow with nothing in flight. */
export function impactBody(overrides: Record<string, unknown> = {}) {
  return {
    valid: true,
    issues: [],
    next_version: 1,
    audience: null,
    enrollments_staying_on_older_versions: 0,
    unhealthy_inputs: [],
    ...overrides,
  };
}

/** One published version of the sample flow. */
export function versionBody(version: number, overrides: Record<string, unknown> = {}) {
  return {
    version,
    graph: sampleGraph(),
    settings: sampleSettings(),
    published_at: "2026-10-05T09:00:00Z",
    published_by: null,
    trace_id: "66666666-0000-4000-8000-000000000001",
    ...overrides,
  };
}
