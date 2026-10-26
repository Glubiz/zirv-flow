import { blockLabel, type Block, type BlocksContent, type DocumentStyle, type Section } from "./blocks/document";
import { readDocument, type Editable } from "./model";

export type ChangeTag = "Added" | "Changed" | "Removed";

export interface Change {
  /** Stable within one diff: what the page keys the list and the selection on. */
  key: string;
  tag: ChangeTag;
  /** Where in the message, such as "Section 2 › Text". */
  where: string;
  /** One line about what happened. */
  what: string;
  /** The section or block to mark in the render; absent for a change that has no place in it. */
  markId?: string;
}

const LONGEST = 40;

function clip(text: string): string {
  const flat = text.replace(/\s+/g, " ").trim();
  return flat.length > LONGEST ? `${flat.slice(0, LONGEST - 1)}…` : flat;
}

function same(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (typeof a !== "object" || typeof b !== "object" || a === null || b === null) return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  const left = a as Record<string, unknown>;
  const right = b as Record<string, unknown>;
  const keys = new Set([...Object.keys(left), ...Object.keys(right)]);
  for (const key of keys) {
    if (!same(left[key], right[key])) return false;
  }
  return true;
}

const FIELD_WORDS: Record<string, string> = {
  text: "Text",
  content: "Text",
  label: "Label",
  url: "Link",
  href: "Link",
  alt: "Alt text",
  src: "Image",
  level: "Level",
  align: "Alignment",
  width: "Width",
  color: "Colour",
  height: "Height",
  html: "HTML",
  snippet_id: "Snippet",
  visible_if: "Condition",
  background_color: "Background",
  padding: "Padding",
};

const STYLE_WORDS: Record<keyof DocumentStyle, string> = {
  background_color: "Background",
  content_width: "Width",
  font_family: "Font",
  text_color: "Text colour",
  link_color: "Link colour",
  button_color: "Button colour",
  button_text_color: "Button text colour",
};

/** The names of the fields that differ between two records, once each and in a stable order. */
function changedWords(before: object, after: object, words: Record<string, string>, skip: readonly string[]): string[] {
  const left = before as Record<string, unknown>;
  const right = after as Record<string, unknown>;
  const names: string[] = [];
  for (const key of new Set([...Object.keys(left), ...Object.keys(right)])) {
    if (skip.includes(key) || same(left[key], right[key])) continue;
    const word = words[key] ?? key.replaceAll("_", " ");
    if (!names.includes(word)) names.push(word);
  }
  return names;
}

function sentence(names: readonly string[], verb: string): string {
  const [first, ...rest] = names;
  if (first === undefined) return verb;
  const all = [first, ...rest.map((name) => name.toLowerCase())];
  return `${all.join(", ")} ${verb}`;
}

/** `before → after` for a short text, or a plain statement when either side is long. */
function textChange(label: string, before: string, after: string): string {
  if (before === "") return `${label} set to "${clip(after)}"`;
  if (after === "") return `${label} cleared`;
  return `${clip(before)} → ${clip(after)}`;
}

interface Placed {
  block: Block;
  section: Section;
  sectionNumber: number;
  column: number;
  index: number;
}

function placeBlocks(content: BlocksContent): Placed[] {
  return content.sections.flatMap((section, sectionIndex) =>
    section.columns.flatMap((blocks, column) => blocks.map((block, index) => ({ block, section, sectionNumber: sectionIndex + 1, column, index }))),
  );
}

/** The positions in `sequence` that belong to one longest increasing run; the others are what moved. */
function stayers(sequence: readonly number[]): Set<number> {
  const length: number[] = [];
  const previous: number[] = [];
  let best = -1;
  sequence.forEach((value, at) => {
    length[at] = 1;
    previous[at] = -1;
    for (let before = 0; before < at; before += 1) {
      if ((sequence[before] ?? 0) < value && (length[before] ?? 0) + 1 > (length[at] ?? 0)) {
        length[at] = (length[before] ?? 0) + 1;
        previous[at] = before;
      }
    }
    if (best === -1 || (length[at] ?? 0) > (length[best] ?? 0)) best = at;
  });
  const keep = new Set<number>();
  for (let at = best; at !== -1; at = previous[at] ?? -1) keep.add(at);
  return keep;
}

function container(placed: Placed): string {
  return `${placed.section.id}/${placed.column}`;
}

/** The one short text a block type is mostly about; a change to only that field shows what it was and what it is now. */
const HEADLINE_FIELD: Partial<Record<Block["type"], string>> = { heading: "text", button: "label" };

function blockEdit(before: Block, after: Block, names: readonly string[]): string {
  const field = HEADLINE_FIELD[after.type];
  if (field !== undefined && names.length === 1 && names[0] === FIELD_WORDS[field]) {
    const was = (before as unknown as Record<string, unknown>)[field];
    const now = (after as unknown as Record<string, unknown>)[field];
    if (typeof was === "string" && typeof now === "string") return textChange(names[0] ?? "Text", was, now);
  }
  return sentence(names, "changed");
}

function diffBlocks(live: BlocksContent, draft: BlocksContent): Change[] {
  const changes: Change[] = [];
  const liveSections = new Map(live.sections.map((section, index) => [section.id, { section, number: index + 1 }]));
  const draftSections = new Map(draft.sections.map((section, index) => [section.id, { section, number: index + 1 }]));
  const liveBlocks = new Map(placeBlocks(live).map((placed) => [placed.block.id, placed]));
  const draftPlaced = placeBlocks(draft);

  // A block that stayed in its section column moved only if it left the longest run that kept its order.
  const moved = new Set<string>();
  const byContainer = new Map<string, Placed[]>();
  for (const placed of draftPlaced) {
    const before = liveBlocks.get(placed.block.id);
    if (before === undefined) continue;
    if (container(before) !== container(placed)) moved.add(placed.block.id);
    else byContainer.set(container(placed), [...(byContainer.get(container(placed)) ?? []), placed]);
  }
  for (const group of byContainer.values()) {
    const order = group.map((placed) => liveBlocks.get(placed.block.id)?.index ?? 0);
    const keep = stayers(order);
    group.forEach((placed, at) => {
      if (!keep.has(at)) moved.add(placed.block.id);
    });
  }

  draft.sections.forEach((section, index) => {
    const before = liveSections.get(section.id);
    const where = `Section ${index + 1}`;
    if (before === undefined) {
      const count = section.columns.reduce((total, blocks) => total + blocks.length, 0);
      changes.push({ key: `section:${section.id}`, tag: "Added", where, what: count === 0 ? "New section" : `New section with ${count} ${count === 1 ? "block" : "blocks"}`, markId: section.id });
      return;
    }
    const names = changedWords({ ...before.section, columns: [] }, { ...section, columns: [] }, FIELD_WORDS, ["id", "columns"]);
    if (names.length > 0) changes.push({ key: `section:${section.id}`, tag: "Changed", where, what: sentence(names, "changed"), markId: section.id });
    else if (before.section.columns.length !== section.columns.length) {
      changes.push({ key: `section:${section.id}`, tag: "Changed", where, what: "Columns changed", markId: section.id });
    }
  });

  for (const placed of draftPlaced) {
    const where = `Section ${placed.sectionNumber} › ${blockLabel(placed.block.type)}`;
    const before = liveBlocks.get(placed.block.id);
    const key = `block:${placed.block.id}`;
    if (before === undefined) {
      // A block of a new section is told with its section.
      if (liveSections.has(placed.section.id)) changes.push({ key, tag: "Added", where, what: `New ${blockLabel(placed.block.type).toLowerCase()}`, markId: placed.block.id });
      continue;
    }
    const names = changedWords(before.block, placed.block, FIELD_WORDS, ["id", "type"]);
    const wasMoved = moved.has(placed.block.id);
    if (names.length === 0 && !wasMoved) continue;
    const parts: string[] = [];
    if (names.length > 0) parts.push(blockEdit(before.block, placed.block, names));
    if (wasMoved) {
      parts.push(before.sectionNumber === placed.sectionNumber ? "moved within the section" : `moved from section ${before.sectionNumber} to section ${placed.sectionNumber}`);
    }
    const what = parts.join("; ");
    changes.push({ key, tag: "Changed", where, what: what.charAt(0).toUpperCase() + what.slice(1), markId: placed.block.id });
  }

  for (const [id, { section, number }] of liveSections) {
    if (draftSections.has(id)) continue;
    const count = section.columns.reduce((total, blocks) => total + blocks.length, 0);
    changes.push({ key: `section:${id}`, tag: "Removed", where: `Section ${number}`, what: count === 0 ? "Section removed" : `Section removed with ${count} ${count === 1 ? "block" : "blocks"}`, markId: id });
  }
  const draftIds = new Set(draftPlaced.map((placed) => placed.block.id));
  for (const [id, placed] of liveBlocks) {
    if (draftIds.has(id) || !draftSections.has(placed.section.id)) continue;
    changes.push({ key: `block:${id}`, tag: "Removed", where: `Section ${placed.sectionNumber} › ${blockLabel(placed.block.type)}`, what: `${blockLabel(placed.block.type)} removed`, markId: id });
  }
  return changes;
}

function theme(value: unknown): string {
  if (typeof value === "object" && value !== null && "id" in value) return "a chosen theme";
  return typeof value === "string" ? value : "default";
}

function diffMessage(live: BlocksContent, draft: BlocksContent): Change[] {
  const changes: Change[] = [];
  const line = (key: string, where: string, what: string) => changes.push({ key, tag: "Changed", where, what });
  if (live.subject !== draft.subject) line("subject", "Subject", textChange("Subject", live.subject, draft.subject));
  if (live.preheader !== draft.preheader) line("preheader", "Preheader", textChange("Preheader", live.preheader, draft.preheader));
  if ((live.sender ?? "") !== (draft.sender ?? "")) line("sender", "Sender", "Sender changed");
  if ((live.lang ?? "") !== (draft.lang ?? "")) line("lang", "Language", textChange("Language", live.lang ?? "", draft.lang ?? ""));
  if (live.category !== draft.category) line("category", "Category", `${live.category} → ${draft.category}`);
  if (!same(live.theme, draft.theme)) line("theme", "Theme", `${theme(live.theme)} → ${theme(draft.theme)}`);
  if (!same(live.footer, draft.footer)) line("footer", "Footer", "Footer choice changed");
  const style = changedWords(live.settings, draft.settings, STYLE_WORDS, []);
  if (style.length > 0) line("settings", "Document style", sentence(style, "changed"));
  return changes;
}

function diffCode(live: Editable, draft: Editable): Change[] {
  const changes: Change[] = [];
  const line = (key: string, where: string, what: string) => changes.push({ key, tag: "Changed", where, what });
  if (live.kind === "email" && draft.kind === "email") {
    if (live.email.subject !== draft.email.subject) line("subject", "Subject", textChange("Subject", live.email.subject, draft.email.subject));
    if (live.email.source !== draft.email.source) line("source", draft.mode === "mjml" ? "MJML" : "HTML", "The source changed");
    if (live.email.text !== draft.email.text) line("text", "Text part", "The text part changed");
    if (live.email.sender !== draft.email.sender) line("sender", "Sender", "Sender changed");
    if (live.email.lang !== draft.email.lang) line("lang", "Language", textChange("Language", live.email.lang, draft.email.lang));
    return changes;
  }
  if (live.kind === "webhook" && draft.kind === "webhook") {
    if (!same(live.webhook.headers, draft.webhook.headers)) line("headers", "Headers", "The headers changed");
    if (live.webhook.body !== draft.webhook.body) line("body", "Body", "The body changed");
    return changes;
  }
  if (!same(live.rest, draft.rest)) line("document", "Template", "The template changed");
  return changes;
}

function writtenAs(editable: Editable): string {
  if (editable.kind === "blocks") return "blocks";
  if (editable.kind === "email") return editable.mode === "mjml" ? "MJML" : "HTML";
  return editable.kind === "webhook" ? "a webhook" : "an unknown form";
}

/**
 * What the draft changes against the live version: the subject, the message settings, the theme choice, and the sections and blocks by id.
 * A block that is in both is Changed (with where it went when it moved), never a removal plus an addition. Pure: it reads two stored documents.
 */
export function diffDocuments(live: unknown, draft: unknown): Change[] {
  const before = readDocument(live);
  const after = readDocument(draft);
  if (before.kind === "blocks" && after.kind === "blocks") return [...diffMessage(before.content, after.content), ...diffBlocks(before.content, after.content)];
  if (before.kind !== after.kind || (before.kind === "email" && after.kind === "email" && before.mode !== after.mode)) {
    return [{ key: "document", tag: "Changed", where: "Template", what: `Written as ${writtenAs(after)} now, was ${writtenAs(before)}` }];
  }
  return diffCode(before, after);
}
