/**
 * The manifest of an `http` source as text: reading it without echoing any of it, finding the
 * line of a JSON pointer so an issue can be shown in place, and starter manifests.
 * Pure: no network, no DOM.
 */

export interface SyntaxProblem {
  /** 1-based. */
  line: number;
  /** 1-based. */
  column: number;
}

export type Analysis =
  | { ok: true; value: unknown; lines: Map<string, number> }
  | { ok: false; problem: SyntaxProblem };

class Stop extends Error {
  constructor(readonly at: number) {
    super("syntax");
  }
}

const MAX_NESTING = 64;

/** The 1-based line and column of an offset in `text`. */
function locate(text: string, offset: number): SyntaxProblem {
  let line = 1;
  let start = 0;
  for (let at = 0; at < offset && at < text.length; at += 1) {
    if (text.charCodeAt(at) === 10) {
      line += 1;
      start = at + 1;
    }
  }
  return { line, column: offset - start + 1 };
}

function escapeSegment(key: string): string {
  return key.replace(/~/g, "~0").replace(/\//g, "~1");
}

/**
 * Reads `text` as JSON. On success, the value and the line each JSON pointer starts on; on
 * failure, only where the first problem is. The text, the token and the browser's own message
 * are never part of the result, so nothing a person typed can end up in a banner or a log.
 */
export function analyze(text: string): Analysis {
  const lines = new Map<string, number>();
  const starts: number[] = [0];
  for (let at = 0; at < text.length; at += 1) {
    if (text.charCodeAt(at) === 10) starts.push(at + 1);
  }
  const lineOf = (offset: number) => {
    let low = 0;
    let high = starts.length - 1;
    while (low < high) {
      const middle = Math.ceil((low + high) / 2);
      if ((starts[middle] ?? 0) <= offset) low = middle;
      else high = middle - 1;
    }
    return low + 1;
  };
  let i = 0;
  const skip = () => {
    while (i < text.length && " \t\r\n".includes(text.charAt(i))) i += 1;
  };
  const expect = (char: string) => {
    if (text[i] !== char) throw new Stop(i);
    i += 1;
  };
  const string = (): string => {
    expect('"');
    const begin = i;
    while (i < text.length && text[i] !== '"') {
      if (text.charCodeAt(i) < 0x20) throw new Stop(i);
      i += text[i] === "\\" ? 2 : 1;
    }
    if (i >= text.length) throw new Stop(i);
    i += 1;
    try {
      return JSON.parse(text.slice(begin - 1, i)) as string;
    } catch {
      throw new Stop(begin);
    }
  };
  const value = (path: string, depth: number): void => {
    if (depth > MAX_NESTING) throw new Stop(i);
    skip();
    lines.set(path, lineOf(i));
    const char = text[i];
    if (char === "{") {
      i += 1;
      skip();
      if (text[i] === "}") {
        i += 1;
        return;
      }
      for (;;) {
        skip();
        const keyAt = i;
        const key = string();
        const child = `${path}/${escapeSegment(key)}`;
        lines.set(child, lineOf(keyAt));
        skip();
        expect(":");
        value(child, depth + 1);
        skip();
        if (text[i] === ",") {
          i += 1;
          continue;
        }
        expect("}");
        return;
      }
    }
    if (char === "[") {
      i += 1;
      skip();
      if (text[i] === "]") {
        i += 1;
        return;
      }
      for (let index = 0; ; index += 1) {
        value(`${path}/${index}`, depth + 1);
        skip();
        if (text[i] === ",") {
          i += 1;
          continue;
        }
        expect("]");
        return;
      }
    }
    if (char === '"') {
      string();
      return;
    }
    const literal = /true|false|null|-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?/y;
    literal.lastIndex = i;
    const found = literal.exec(text);
    if (!found) throw new Stop(i);
    i += found[0].length;
  };
  try {
    value("", 0);
    skip();
    if (i < text.length) throw new Stop(i);
  } catch (error) {
    if (error instanceof Stop) return { ok: false, problem: locate(text, error.at) };
    throw error;
  }
  return { ok: true, value: JSON.parse(text) as unknown, lines };
}

/** The line a pointer starts on, or the nearest enclosing key's line when the key is missing. */
export function lineOfPointer(lines: Map<string, number>, pointer: string): number {
  let at = pointer;
  for (;;) {
    const line = lines.get(at);
    if (line !== undefined) return line;
    const cut = at.lastIndexOf("/");
    if (cut < 0) return 1;
    at = at.slice(0, cut);
  }
}

/** The offset of the first character of a 1-based line. */
export function offsetOfLine(text: string, line: number): number {
  let offset = 0;
  for (let current = 1; current < line; current += 1) {
    const next = text.indexOf("\n", offset);
    if (next < 0) return text.length;
    offset = next + 1;
  }
  return offset;
}

/** The text indented, or null when it is not JSON. */
export function prettyPrint(text: string): string | null {
  const analysis = analyze(text);
  return analysis.ok ? JSON.stringify(analysis.value, null, 2) : null;
}

export type AuthType =
  | "none"
  | "api_key"
  | "basic"
  | "bearer"
  | "oauth2_client_credentials"
  | "oauth2_authorization_code";

export const STARTERS: { id: AuthType; label: string }[] = [
  { id: "none", label: "No authentication" },
  { id: "api_key", label: "API key" },
  { id: "basic", label: "User name and password" },
  { id: "bearer", label: "Bearer token" },
  { id: "oauth2_client_credentials", label: "OAuth2 client credentials" },
  { id: "oauth2_authorization_code", label: "OAuth2 sign-in (authorization code)" },
];

function starterAuth(type: AuthType): Record<string, unknown> {
  switch (type) {
    case "api_key":
      return { type, in: "header", name: "X-Api-Key" };
    case "oauth2_client_credentials":
      return { type, token_url: "https://api.example.com/oauth/token", scopes: [] };
    case "oauth2_authorization_code":
      return {
        type,
        authorize_url: "https://api.example.com/oauth/authorize",
        token_url: "https://api.example.com/oauth/token",
        scopes: ["contacts.read"],
      };
    default:
      return { type };
  }
}

/** A manifest to start from, with one stream and the given kind of authentication. */
export function starterManifest(type: AuthType): string {
  return JSON.stringify(
    {
      version: "v1",
      base_url: "https://api.example.com",
      auth: starterAuth(type),
      streams: [
        {
          name: "contacts",
          path: "/contacts",
          records_path: "/data",
          pagination: { type: "none" },
          fields: [
            { name: "id", type: "string" },
            { name: "email", type: "string" },
            { name: "updated_at", type: "timestamp" },
          ],
          primary_key: ["id"],
        },
      ],
    },
    null,
    2,
  );
}
