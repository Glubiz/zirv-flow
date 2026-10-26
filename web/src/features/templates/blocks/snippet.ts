import type { Editable } from "../model";
import { emptyContent, readContent, type Block } from "./document";

type Blocks = Extract<Editable, { kind: "blocks" }>;

/** The id of the one section a snippet's blocks sit in while the block editor works on them. */
const SNIPPET_SECTION = "snippet";

/** A snippet's `{"blocks": [...]}` as the block editor holds it: one section with one column. */
export function snippetEditable(content: unknown): Blocks {
  const raw = typeof content === "object" && content !== null ? (content as Record<string, unknown>)["blocks"] : undefined;
  const blocks = Array.isArray(raw) ? raw : [];
  const read = readContent({ sections: [{ id: SNIPPET_SECTION, columns: [blocks] }] });
  return { kind: "blocks", content: { ...emptyContent("marketing"), sections: read.sections }, newer: false, rest: {} };
}

/** The snippet content for what the block editor holds. */
export function snippetContent(doc: Blocks): { blocks: Block[] } {
  return { blocks: doc.content.sections[0]?.columns[0] ?? [] };
}
