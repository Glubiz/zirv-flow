import { json } from "./mockApi";

/** Fixtures and a stand-in for XMLHttpRequest shared by the image library and picker tests. */

export function members(role: "admin" | "editor", userId: string) {
  return [{ user_id: userId, email: "someone@example.test", role, added_at: "2026-10-01T09:00:00Z" }];
}

export function assetBody(n: number, overrides: Record<string, unknown> = {}) {
  const sha = String(n).padStart(2, "0").repeat(32);
  return {
    id: `aaaaaaaa-0000-4000-8000-0000000000${String(n).padStart(2, "0")}`,
    name: `photo-${n}.png`,
    content_type: "image/png",
    size_bytes: 38 * 1024,
    width: 640,
    height: 160,
    sha256: sha,
    url: `/a/${sha}.png`,
    references: 0,
    created_at: "2026-10-12T09:00:00Z",
    ...overrides,
  };
}

export function listBody(assets: unknown[], overrides: Record<string, unknown> = {}) {
  return {
    assets,
    next_before: null,
    used_bytes: 212 * 1024 * 1024,
    limit_bytes: 1024 * 1024 * 1024,
    max_file_bytes: 5 * 1024 * 1024,
    ...overrides,
  };
}

export const listRoute = (body: unknown) => () => json(body);

export class FakeRequest {
  static all: FakeRequest[] = [];
  method = "";
  url = "";
  headers: Record<string, string> = {};
  body: unknown = null;
  status = 0;
  responseText = "";
  upload: { onprogress: ((event: { lengthComputable: boolean; loaded: number; total: number }) => void) | null } = {
    onprogress: null,
  };
  onload: (() => void) | null = null;
  onerror: (() => void) | null = null;
  onabort: (() => void) | null = null;
  aborted = false;

  constructor() {
    FakeRequest.all.push(this);
  }

  open(method: string, url: string) {
    this.method = method;
    this.url = url;
  }

  setRequestHeader(name: string, value: string) {
    this.headers[name.toLowerCase()] = value;
  }

  send(body: unknown) {
    this.body = body;
  }

  abort() {
    this.aborted = true;
    this.onabort?.();
  }

  progress(loaded: number, total: number) {
    this.upload.onprogress?.({ lengthComputable: true, loaded, total });
  }

  respond(status: number, body: unknown) {
    this.status = status;
    this.responseText = JSON.stringify(body);
    this.onload?.();
  }
}
