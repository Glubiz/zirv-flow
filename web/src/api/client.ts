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

let onUnauthorized: (() => void) | null = null;

export function setUnauthorizedHandler(handler: (() => void) | null): void {
  onUnauthorized = handler;
}

const OWN_STATUS_PATHS = ["/api/v1/session", "/api/v1/setup"];

const expiry: Middleware = {
  onResponse({ request, response }) {
    const path = new URL(request.url).pathname;
    if (response.status === 401 && !OWN_STATUS_PATHS.some((own) => path.startsWith(own))) {
      onUnauthorized?.();
    }
    return response;
  },
};

export const client = createClient<paths>({
  baseUrl: globalThis.location.origin,
  credentials: "same-origin",
  fetch: (request) => globalThis.fetch(request),
});
client.use(csrf);
client.use(expiry);

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
