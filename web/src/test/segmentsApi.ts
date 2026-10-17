import { json, MEMBER, OWNER, session, type Handler } from "./mockApi";

/** Fixtures and the catalog routes shared by the segment tests. */

export const WS = "/api/v1/workspaces/ws1";

export const SEGMENT_ID = "22222222-0000-4000-8000-000000000001";
export const OTHER_SEGMENT_ID = "22222222-0000-4000-8000-000000000002";
export const LIST_ID = "11111111-0000-4000-8000-000000000001";
export const PERSON_ID = "33333333-0000-4000-8000-000000000001";

export function trait(key: string, type: string) {
  return {
    key,
    type,
    archived: false,
    channel_address: false,
    created_at: "2026-10-01T09:00:00Z",
    description: "",
    inferred: false,
    pii: false,
    precedence: "latest",
    source_order: [],
    updated_at: "2026-10-01T09:00:00Z",
  };
}

export const TRAITS = [
  trait("city", "string"),
  trait("age", "integer"),
  trait("vip", "boolean"),
  trait("joined_at", "timestamp"),
  trait("tags", "string_list"),
];

export const EVENTS = [
  {
    name: "purchase",
    created_at: "2026-10-01T09:00:00Z",
    description: "",
    has_draft: false,
    inferred: false,
    mode: "strict",
    updated_at: "2026-10-01T09:00:00Z",
    current_version: 1,
  },
];

export const PURCHASE_DETAIL = {
  schema: EVENTS[0],
  versions: [
    {
      version: 1,
      published_at: "2026-10-01T09:00:00Z",
      properties: [
        { name: "amount", type: "number" },
        { name: "currency", type: "string" },
      ],
    },
  ],
};

export const LISTS = [
  {
    id: LIST_ID,
    name: "Newsletter",
    description: "",
    member_count: 3,
    created_at: "2026-10-01T09:00:00Z",
    updated_at: "2026-10-01T09:00:00Z",
  },
];

export const TOPICS = [
  {
    id: "44444444-0000-4000-8000-000000000001",
    key: "offers",
    name: "Offers",
    channel: "email",
    archived: false,
    created_at: "2026-10-01T09:00:00Z",
    description: "",
    policy: "opt_in",
    updated_at: "2026-10-01T09:00:00Z",
  },
];

export function segmentRow(overrides: Record<string, unknown> = {}) {
  return {
    id: SEGMENT_ID,
    name: "Big spenders",
    description: "People who spend",
    cadence_minutes: 60,
    computed_at: "2026-10-10T09:00:00Z",
    computed_version: 1,
    member_count: 7,
    created_at: "2026-10-01T09:00:00Z",
    updated_at: "2026-10-01T09:00:00Z",
    current_version: 1,
    ...overrides,
  };
}

export const VERSION_1 = {
  version: 1,
  created_at: "2026-10-01T09:00:00Z",
  created_by: null,
  definition: { v: 1, all: [{ trait: "city", op: "eq", value: "Aarhus" }] },
  deps: {},
  time_sensitive: false,
};

export function detail(overrides: Record<string, unknown> = {}, version: Record<string, unknown> = {}) {
  const current = { ...VERSION_1, ...version };
  return { ...segmentRow({ current_version: current["version"] }), ...overrides, current };
}

export const LIVE_AGE = { live: true, as_of: null, sources: [] };

export function previewBody(count = 4, overrides: Record<string, unknown> = {}) {
  return {
    count,
    sample: [{ id: PERSON_ID, display: { kind: "email", masked: false, value: "ada@example.test" } }],
    data_age: LIVE_AGE,
    compiled_at: "2026-10-10T09:00:00Z",
    ...overrides,
  };
}

export function members(role: "admin" | "member" | null) {
  return role === null ? [] : [{ user_id: MEMBER.id, email: MEMBER.email, role, added_at: "2026-10-01T09:00:00Z" }];
}

/** The session and access routes: the owner writes, a bare member of the session user reads only. */
export function actor(kind: "writer" | "reader"): Record<string, Handler> {
  return kind === "writer"
    ? {
        "GET /api/v1/session": () => json(session(OWNER)),
        "GET /api/v1/workspaces/ws1/members": () => json(members("admin")),
      }
    : {
        "GET /api/v1/session": () => json(session(MEMBER)),
        "GET /api/v1/workspaces/ws1/members": () => json(members(null)),
      };
}

/** The routes the builder reads to offer its choices. */
export function catalogRoutes(extra: { segments?: unknown[] } = {}): Record<string, Handler> {
  return {
    [`GET ${WS}/traits`]: () => json(TRAITS),
    [`GET ${WS}/event-schemas`]: () => json(EVENTS),
    [`GET ${WS}/event-schemas/purchase`]: () => json(PURCHASE_DETAIL),
    [`GET ${WS}/lists`]: () => json(LISTS),
    [`GET ${WS}/segments`]: () => json(extra.segments ?? [segmentRow(), segmentRow({ id: OTHER_SEGMENT_ID, name: "Lapsed" })]),
    [`GET ${WS}/topics`]: () => json(TOPICS),
  };
}

/** A problem+json answer carrying a stable code and, for a refused definition, its issues. */
export function refusal(status: number, code: string, issues: { pointer: string; code: string }[] = []): Response {
  return new Response(
    JSON.stringify({ type: "about:blank", title: "Refused", status, detail: "The server refused: " + code, code, issues }),
    { status, headers: { "content-type": "application/problem+json" } },
  );
}

/** The definitions the preview endpoint was asked about, oldest first. */
export function previewed(calls: { method: string; path: string; body: unknown }[]): unknown[] {
  return calls
    .filter((call) => call.method === "POST" && call.path === `${WS}/segments/preview`)
    .map((call) => (call.body as { definition: unknown }).definition);
}
