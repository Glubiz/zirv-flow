import { vi } from "vitest";

export type Handler = (request: Request) => Response | Promise<Response>;

export interface Call {
  method: string;
  path: string;
  search: string;
  headers: Headers;
  body: unknown;
}

export function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
}

export function problem(status: number, title: string, detail: string, traceId?: string, code?: string): Response {
  return new Response(
    JSON.stringify({ type: "about:blank", title, status, detail, trace_id: traceId ?? null, request_id: null, ...(code ? { code } : {}) }),
    { status, headers: { "content-type": "application/problem+json" } },
  );
}

function parseBody(text: string, contentType: string | null): unknown {
  return contentType?.includes("json") ? (JSON.parse(text) as unknown) : text;
}

export function mockApi(routes: Record<string, Handler>): Call[] {
  const calls: Call[] = [];
  vi.stubGlobal("fetch", async (input: Request | string | URL) => {
    const request = input instanceof Request ? input : new Request(input);
    const path = new URL(request.url).pathname;
    const text = await request.clone().text();
    calls.push({
      method: request.method,
      path,
      search: new URL(request.url).search,
      headers: request.headers,
      body: text === "" ? null : parseBody(text, request.headers.get("content-type")),
    });
    const handler = routes[`${request.method} ${path}`];
    if (!handler) {
      return problem(500, "Unmocked request", `${request.method} ${path}`);
    }
    return handler(request);
  });
  return calls;
}

export const OWNER = { id: "u-owner", email: "owner@example.test", is_owner: true };
export const MEMBER = { id: "u-member", email: "member@example.test", is_owner: false };

export function session(user: typeof OWNER, csrf = "csrf-test-token") {
  return { user, csrf_token: csrf };
}

export const WORKSPACE = { id: "ws1", name: "Spring campaigns", slug: "spring", created_at: "2026-10-01T09:00:00Z" };
