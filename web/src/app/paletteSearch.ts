export const GROUP_LIMIT = 5;

export type IdentifierKind = "email" | "phone" | "anonymous_id";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const PHONE = /^\+?[\d\s().-]+$/;

/** What a query looks like as a person identifier, or null when it is only a name to look for. */
export function identifierKind(query: string): IdentifierKind | null {
  const value = query.trim();
  if (value.includes("@") && !/\s/.test(value) && value.length >= 3) return "email";
  if (UUID.test(value)) return "anonymous_id";
  if (PHONE.test(value) && value.replace(/\D/g, "").length >= 3) return "phone";
  return null;
}

/** Case-insensitive substring matches, names that start with the query first, at most `limit`. */
export function matchNames<T extends { name: string }>(items: readonly T[], query: string, limit = GROUP_LIMIT): T[] {
  const needle = query.trim().toLocaleLowerCase();
  if (needle === "") return [];
  const prefix: T[] = [];
  const inside: T[] = [];
  for (const item of items) {
    const name = item.name.toLocaleLowerCase();
    if (name.startsWith(needle)) prefix.push(item);
    else if (name.includes(needle)) inside.push(item);
  }
  return [...prefix, ...inside].slice(0, limit);
}
