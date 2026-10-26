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

/**
 * Sends `file` as the raw body of a POST, with the CSRF token, and reports how much has gone
 * out. `fetch` cannot report upload progress, so this one call uses XMLHttpRequest. Resolves
 * with the parsed JSON answer; rejects with an ApiError for a problem answer. Aborting `options.signal`
 * cancels the request and rejects; `options.onStatus` is told the status of a successful answer.
 */
export function uploadFile<T>(
  url: string,
  file: Blob,
  contentType: string,
  onProgress?: (sent: number, total: number) => void,
  options: { signal?: AbortSignal; onStatus?: (status: number) => void } = {},
): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const request = new XMLHttpRequest();
    options.signal?.addEventListener("abort", () => request.abort(), { once: true });
    request.open("POST", url);
    request.setRequestHeader("Content-Type", contentType);
    if (csrfToken !== null) {
      request.setRequestHeader(CSRF_HEADER, csrfToken);
    }
    request.upload.onprogress = (event) => {
      if (event.lengthComputable) {
        onProgress?.(event.loaded, event.total);
      }
    };
    request.onerror = () => reject(new Error("The upload did not reach the server. Check your connection and try again."));
    request.onabort = () => reject(new Error("The upload was cancelled."));
    request.onload = () => {
      let body: unknown;
      try {
        body = JSON.parse(request.responseText) as unknown;
      } catch {
        body = request.responseText;
      }
      if (request.status >= 200 && request.status < 300) {
        options.onStatus?.(request.status);
        resolve(body as T);
        return;
      }
      if (request.status === 401) {
        onUnauthorized?.();
      }
      reject(problemFrom(body, new Response(null, { status: request.status === 0 ? 500 : request.status })));
    };
    if (options.signal?.aborted) {
      reject(new Error("The upload was cancelled."));
      return;
    }
    request.send(file);
  });
}
