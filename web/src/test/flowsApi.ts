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
  role?: "writer" | "publisher" | "editor" | "reader";
  flow?: Record<string, unknown>;
  /** The issues the validate endpoint reports, or a function of how many times it was asked. */
  issues?: Issue[] | ((asked: number) => Issue[]);
}

/** The session, the catalog, the flow and its save and validate endpoints. PATCH applies what it is sent. */
export function flowRoutes({ role = "writer", flow, issues = [] }: Options = {}): Record<string, Handler> {
  let current: Record<string, unknown> = flowDetail(flow);
  let asked = 0;
  return {
    ...(role === "editor" || role === "publisher"
      ? {
          "GET /api/v1/session": () => json(session(MEMBER)),
          "GET /api/v1/workspaces/ws1/members": () => json(members(role)),
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

/** The assistant's settings with the assistant switched `enabled` or not. */
export function aiSettings(enabled: boolean) {
  return {
    enabled,
    provider_id: "openai",
    model: "big",
    monthly_budget_micros: 1_000_000,
    pii_opt_in: false,
    transcript_days: null,
    provider_key_set: true,
    last_probe: null,
    updated_at: "2026-10-01T09:00:00Z",
  };
}

export const TEMPLATE_ID = "77777777-0000-4000-8000-000000000001";
export const HOOK_TEMPLATE_ID = "77777777-0000-4000-8000-000000000002";
export const DESTINATION_ID = "99999999-0000-4000-8000-000000000001";
export const WEBHOOK_DESTINATION_ID = "99999999-0000-4000-8000-000000000002";
export const PROFILE_DESTINATION_ID = "99999999-0000-4000-8000-000000000003";
export const ARCHIVED_DESTINATION_ID = "99999999-0000-4000-8000-000000000004";

/** What the writers' destination read returns: one that sends and syncs, one that does neither, one that only syncs, an archived one. */
export function destinationOptions() {
  return [
    { id: DESTINATION_ID, name: "Transactional mail", archived: false, capabilities: { send: true, sync: true, erase: false } },
    { id: WEBHOOK_DESTINATION_ID, name: "Order hook", archived: false, capabilities: { send: false, sync: false, erase: false } },
    { id: PROFILE_DESTINATION_ID, name: "Profile store", archived: false, capabilities: { send: false, sync: true, erase: false } },
    { id: ARCHIVED_DESTINATION_ID, name: "Old mail", archived: true, capabilities: { send: true, sync: true, erase: false } },
  ];
}
export const TOPIC_ID = "44444444-0000-4000-8000-000000000001";

/** A wait, a send, a profile sync and an exit, wired in a line. */
export function sendGraph() {
  return {
    v: 1,
    nodes: [
      { id: "n1", type: "wait.duration", seconds: 86_400 },
      { id: "n2", type: "action.send", template: TEMPLATE_ID, destination: DESTINATION_ID, topic: TOPIC_ID, dimensions: { vertical: "retail" } },
      { id: "n3", type: "action.sync", destination: DESTINATION_ID, operation: "upsert", trait_keys: ["city"] },
      { id: "n4", type: "action.webhook", url: "https://crm.example.com/hooks", template: HOOK_TEMPLATE_ID, trait_keys: [] },
      { id: "n5", type: "exit" },
    ],
    edges: [
      { id: "e1", from: "n1", port: "next", to: "n2" },
      { id: "e2", from: "n2", port: "next", to: "n3" },
      { id: "e3", from: "n3", port: "next", to: "n4" },
      { id: "e4", from: "n4", port: "next", to: "n5" },
    ],
  };
}

function publishedTemplate(id: string, name: string, channel: string) {
  return {
    id,
    name,
    channel,
    category: channel === "email" ? "marketing" : "transactional",
    current_version: 1,
    archived_at: null,
    created_at: "2026-10-01T09:00:00Z",
    updated_at: "2026-10-01T09:00:00Z",
    created_by: null,
    sender_identity_id: null,
  };
}

/** The templates, destinations and consent dimensions a send, sync or webhook step offers to choose from. */
export function deliveryRoutes(): Record<string, Handler> {
  return {
    [`GET ${WS}/templates`]: () =>
      json([
        publishedTemplate(TEMPLATE_ID, "Trial nudge", "email"),
        publishedTemplate("77777777-0000-4000-8000-000000000003", "Unpublished", "email"),
        publishedTemplate(HOOK_TEMPLATE_ID, "CRM payload", "webhook"),
      ].map((entry) => (entry.name === "Unpublished" ? { ...entry, current_version: null } : entry))),
    [`GET ${WS}/destinations/options`]: () => json(destinationOptions()),
    [`GET ${WS}/consent/dimensions`]: () =>
      json([
        {
          key: "vertical",
          name: "Vertical",
          archived: false,
          builtin: false,
          values: [
            { key: "retail", name: "Retail", archived: false },
            { key: "b2b", name: "B2B", archived: false },
          ],
        },
      ]),
  };
}
