import { json, MEMBER, OWNER, problem, session, type Call, type Handler } from "./mockApi";
import { listBody } from "./assetsApi";
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
    hidden_blocks: [],
    ...overrides,
  };
}

export function useBody(overrides: Doc = {}) {
  return { flow_id: FLOW_ID, flow_name: "Welcome series", flow_state: "published", active: true, versions: [1], ...overrides };
}

/** What the writers' sender read returns: no domain, no reply-to. */
export const SENDER_OPTIONS = [{ id: SENDER_ID, name: "Acme", from_address: "hello@mail.acme.example", archived: false }];

/** What the writers' destination read returns: one that sends, one that cannot, and an archived one. */
export const DESTINATION_OPTIONS = [
  { id: DESTINATION_ID, name: "Transactional mail", archived: false, capabilities: { send: true, sync: false, erase: false } },
  { id: "99999999-0000-4000-8000-000000000002", name: "Order hook", archived: false, capabilities: { send: false, sync: false, erase: false } },
  { id: "99999999-0000-4000-8000-000000000003", name: "Old mail", archived: true, capabilities: { send: true, sync: false, erase: false } },
];

interface Options {
  /** `admin` publishes and previews real people; `member` edits; `reader` only looks. */
  role?: "admin" | "member" | "reader";
  detail?: Record<string, unknown>;
  /** What a preview answers for the saved draft, or a response to refuse it with. */
  preview?: (document: Doc, asked: number, body: Record<string, unknown>) => Record<string, unknown> | Response;
  uses?: unknown[];
  versions?: unknown[];
  /** The people saved on the template, as the preview-people list answers them. */
  saved?: unknown[];
  /** Routes added to, or replacing, the defaults: the workspace's themes, snippets and images answer empty lists. */
  extra?: Record<string, Handler>;
}

/** The session, the catalog, the template and its draft, preview, publish, versions and where-used endpoints. */
export function templateRoutes({ role = "admin", detail, preview, uses = [], versions = [], saved = [], extra = {} }: Options = {}): Record<string, Handler> {
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
    [`GET ${base}/preview-people`]: () => json(saved),
    [`POST ${base}/preview-people`]: async (request) => json(await request.json(), 201),
    [`DELETE ${base}/preview-people/p-1`]: () => new Response(null, { status: 204 }),
    [`GET ${WS}/sender-identities/options`]: () => json(SENDER_OPTIONS),
    [`GET ${WS}/destinations/options`]: () => json(DESTINATION_OPTIONS),
    [`GET ${WS}/test-send-allowlist/options`]: () => json([{ id: "a1", name: "q***@example.com" }]),
    [`GET ${WS}/test-sends`]: () => json({ test_sends: [], next_before: null }),
    [`GET ${WS}/themes`]: () => json([]),
    [`GET ${WS}/snippets`]: () => json([]),
    [`GET ${WS}/assets`]: () => json(listBody([])),
    ...extra,
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
