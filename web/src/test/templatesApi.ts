import { json, MEMBER, OWNER, problem, session, type Call, type Handler } from "./mockApi";
import { catalogRoutes, members, WS } from "./segmentsApi";

/** Fixtures and routes shared by the template tests. */

export { WS };

export const TEMPLATE_ID = "77777777-0000-4000-8000-000000000001";
export const SENDER_ID = "88888888-0000-4000-8000-000000000001";
export const DESTINATION_ID = "99999999-0000-4000-8000-000000000001";
export const FLOW_ID = "55555555-0000-4000-8000-000000000001";

export type Doc = Record<string, unknown>;

export function emailDocument(overrides: Doc = {}, email: Doc = {}): Doc {
  return {
    schema: 1,
    channel: "email",
    mode: "code",
    email: { subject: "Hello", html: "<p>Hi {{ person.city }}</p>", text: "Hi", category: "marketing", sender: SENDER_ID, ...email },
    ...overrides,
  };
}

export function templateBody(overrides: Doc = {}) {
  return {
    id: TEMPLATE_ID,
    name: "Trial nudge",
    channel: "email",
    category: "marketing",
    current_version: null,
    archived_at: null,
    created_at: "2026-10-01T09:00:00Z",
    updated_at: "2026-10-10T09:00:00Z",
    created_by: null,
    sender_identity_id: SENDER_ID,
    utm: "inherit",
    ...overrides,
  };
}

export function templateDetail(overrides: Doc = {}, document: Doc = emailDocument()) {
  return {
    ...templateBody(overrides),
    draft: { content_type: "html", document, revision: 1, updated_at: "2026-10-10T09:00:00Z", updated_by: null },
  };
}

export interface Issue {
  code: string;
  severity: "blocking" | "warning";
  at: string;
  field?: string | null;
  message?: string | null;
}

export function previewBody(overrides: Doc = {}) {
  return {
    source: "draft",
    version: null,
    content_type: "html",
    person_id: null,
    rendered: true,
    subject: "Hello",
    html: "<p>Hi Aarhus</p>",
    text: "Hi Aarhus",
    headers: [],
    body: null,
    issues: [],
    ...overrides,
  };
}

export function useBody(overrides: Doc = {}) {
  return { flow_id: FLOW_ID, flow_name: "Welcome series", flow_state: "published", active: true, versions: [1], ...overrides };
}

export const SENDERS = [
  {
    id: SENDER_ID,
    from_name: "Acme",
    from_address: "hello@mail.acme.example",
    domain_id: "dddddddd-0000-4000-8000-000000000001",
    archived: false,
    created_at: "2026-10-01T09:00:00Z",
    updated_at: "2026-10-01T09:00:00Z",
    reply_to: null,
  },
];

export const DESTINATIONS = [
  {
    id: DESTINATION_ID,
    name: "Transactional mail",
    kind: "smtp",
    archived: false,
    burst: 10,
    rate: 5,
    config: {},
    credential_fields_set: [],
    created_at: "2026-10-01T09:00:00Z",
    updated_at: "2026-10-01T09:00:00Z",
  },
];

interface Options {
  /** `admin` publishes and previews real people; `member` edits; `reader` only looks. */
  role?: "admin" | "member" | "reader";
  detail?: Record<string, unknown>;
  /** What a preview answers for the saved draft, or a response to refuse it with. */
  preview?: (document: Doc, asked: number, body: Record<string, unknown>) => Record<string, unknown> | Response;
  uses?: unknown[];
  versions?: unknown[];
}

/** The session, the catalog, the template and its draft, preview, publish, versions and where-used endpoints. */
export function templateRoutes({ role = "admin", detail, preview, uses = [], versions = [] }: Options = {}): Record<string, Handler> {
  let current: Record<string, unknown> = detail ?? templateDetail();
  let asked = 0;
  const base = `${WS}/templates/${TEMPLATE_ID}`;
  const actorRoutes: Record<string, Handler> =
    role === "admin"
      ? {
          "GET /api/v1/session": () => json(session(OWNER)),
          "GET /api/v1/workspaces/ws1/members": () => json(members("admin")),
        }
      : {
          "GET /api/v1/session": () => json(session(MEMBER)),
          "GET /api/v1/workspaces/ws1/members": () => json(members(role === "member" ? "member" : null)),
        };
  return {
    ...actorRoutes,
    ...catalogRoutes(),
    [`GET ${WS}/templates`]: () => json([current]),
    [`GET ${base}`]: () => json(current),
    [`PUT ${base}`]: async (request) => {
      const body = (await request.json()) as Doc;
      current = { ...current, ...body };
      return json(current);
    },
    [`PUT ${base}/draft`]: async (request) => {
      const body = (await request.json()) as { document: Doc };
      current = { ...current, draft: { ...(current["draft"] as Doc), document: body.document } };
      return json(current);
    },
    [`POST ${base}/preview`]: async (request) => {
      asked += 1;
      const body = (await request.json()) as Record<string, unknown>;
      const document = (current["draft"] as { document: Doc }).document;
      const answer = preview ? preview(document, asked, body) : previewBody();
      return answer instanceof Response ? answer : json(answer);
    },
    [`POST ${base}/publish`]: () => {
      const next = ((current["current_version"] as number | null) ?? 0) + 1;
      current = { ...current, current_version: next };
      return json({ template: current, version: { version: next, document: (current["draft"] as Doc)["document"], published_at: "2026-10-11T09:00:00Z", warnings: [] } });
    },
    [`GET ${base}/versions`]: () => json(versions),
    [`GET ${base}/where-used`]: () => json(uses),
    [`GET ${WS}/sender-identities`]: () => json(SENDERS),
    [`GET ${WS}/destinations`]: () => json(DESTINATIONS),
    [`GET ${WS}/test-send-allowlist`]: () => json({ entries: [{ id: "a1", display_hint: "q***@example.com", created_at: "2026-10-01T09:00:00Z" }] }),
    [`GET ${WS}/test-sends`]: () => json({ test_sends: [], next_before: null }),
  };
}

/** The saved documents, oldest first. */
export function drafts(calls: Call[]): Doc[] {
  return calls.filter((call) => call.method === "PUT" && call.path.endsWith("/draft")).map((call) => (call.body as { document: Doc }).document);
}

export function previews(calls: Call[]): Call[] {
  return calls.filter((call) => call.method === "POST" && call.path.endsWith("/preview"));
}

export { problem };
