import createClient, { type Middleware } from "openapi-fetch";

import { problemFrom } from "./errors";
import type { paths } from "./schema";

const CSRF_HEADER = "x-csrf-token";
const SAFE_METHODS = new Set(["GET", "HEAD", "OPTIONS"]);

let csrfToken: string | null = null;

export function setCsrfToken(token: string | null): void {
  csrfToken = token;
}

const csrf: Middleware = {
  onRequest({ request }) {
    if (csrfToken !== null && !SAFE_METHODS.has(request.method.toUpperCase())) {
      request.headers.set(CSRF_HEADER, csrfToken);
    }
    return request;
  },
};

export const client = createClient<paths>({
  baseUrl: globalThis.location.origin,
  credentials: "same-origin",
  fetch: (request) => globalThis.fetch(request),
});
client.use(csrf);

interface Result<T> {
  data?: T;
  error?: unknown;
  response: Response;
}

export async function unwrap<T>(call: Promise<Result<T>>): Promise<T> {
  const { data, error, response } = await call;
  if (!response.ok) {
    throw problemFrom(error, response);
  }
  return data as T;
}

export async function unwrapEmpty(call: Promise<Result<unknown>>): Promise<void> {
  const { error, response } = await call;
  if (!response.ok) {
    throw problemFrom(error, response);
  }
}
