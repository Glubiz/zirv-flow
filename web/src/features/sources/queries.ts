/** A named SELECT a database source offers as a stream (`query:<name>`). */
export interface Query {
  name: string;
  sql: string;
}

/** The limits the server enforces on the `queries` setting. */
export const MAX_QUERIES = 20;
export const MAX_QUERY_NAME_CHARS = 64;
export const MAX_QUERY_SQL_CHARS = 20_000;

const NAME_PATTERN = /^[a-z0-9_]+$/;
const STATEMENT_START = /^(select|with)(\s|\(|$)/i;

/**
 * The queries in a form value, which holds the JSON text of the list. Empty text is no queries;
 * text that is not a list of queries is `null`, which only happens when the value was changed
 * outside the editor.
 */
export function parseQueries(text: string): Query[] | null {
  if (text.trim() === "") return [];
  try {
    const parsed: unknown = JSON.parse(text);
    if (!Array.isArray(parsed)) return null;
    const rows: Query[] = [];
    for (const entry of parsed) {
      if (typeof entry !== "object" || entry === null) return null;
      const { name, sql } = entry as Record<string, unknown>;
      if (typeof name !== "string" || typeof sql !== "string") return null;
      rows.push({ name, sql });
    }
    return rows;
  } catch {
    return null;
  }
}

/** The form value for a list of queries; no queries is empty text, so the setting is left out. */
export function serializeQueries(rows: Query[]): string {
  return rows.length === 0 ? "" : JSON.stringify(rows);
}

/** What is wrong with the name of row `index`, in words for the person, or nothing. */
export function nameProblem(rows: Query[], index: number): string | undefined {
  const name = rows[index]?.name ?? "";
  if (name === "") return "Give the query a name.";
  if (name.length > MAX_QUERY_NAME_CHARS || !NAME_PATTERN.test(name)) {
    return `Use 1 to ${MAX_QUERY_NAME_CHARS} characters: a to z, 0 to 9 and _.`;
  }
  if (rows.some((other, at) => at !== index && other.name === name)) return "Another query has this name.";
  return undefined;
}

/** What is wrong with a query's SQL, in words for the person, or nothing. */
export function sqlProblem(sql: string): string | undefined {
  if (sql.trim() === "") return "Enter the SELECT statement.";
  if (sql.length > MAX_QUERY_SQL_CHARS) return `Keep it under ${MAX_QUERY_SQL_CHARS.toLocaleString("en-US")} characters.`;
  if (sql.trimEnd().endsWith(";")) return "Remove the semicolon at the end.";
  if (!STATEMENT_START.test(sql.trimStart())) return "Start with SELECT or WITH.";
  return undefined;
}

/** Whether a form value holds queries the server will accept the shape of. */
export function queriesValid(text: string): boolean {
  const rows = parseQueries(text);
  if (rows === null || rows.length > MAX_QUERIES) return false;
  return rows.every((row, index) => nameProblem(rows, index) === undefined && sqlProblem(row.sql) === undefined);
}
