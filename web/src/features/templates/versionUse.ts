import { readDocument } from "./model";

/** How a document picks its theme. */
export type ThemeChoice = { kind: "default" } | { kind: "none" } | { kind: "named"; id: string };

/** The theme a document asks for: only a blocks document has one. */
export function themeChoice(document: unknown): ThemeChoice {
  const doc = readDocument(document);
  if (doc.kind !== "blocks") return { kind: "none" };
  const theme = doc.content.theme;
  if (theme === "none") return { kind: "none" };
  if (typeof theme === "object") return { kind: "named", id: theme.id };
  return { kind: "default" };
}

/** The snippets a blocks document names, each once, in the order they first appear. */
export function snippetIds(document: unknown): string[] {
  const doc = readDocument(document);
  if (doc.kind !== "blocks") return [];
  const ids = doc.content.sections.flatMap((section) => section.columns.flat()).flatMap((block) => (block.type === "snippet" ? [block.snippet_id] : []));
  return [...new Set(ids)];
}
