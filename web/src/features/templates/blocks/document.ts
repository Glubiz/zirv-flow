/** The block document, schema 2, as the editor builds it (architecture 11.2). The shape is the server's; nothing here adds a field. */

import { AGG_FNS, type ConditionNode } from "../../segments";

export const BLOCKS_SCHEMA = 2;
export const MAX_SECTIONS = 50;
export const MAX_BLOCKS = 500;
export const MAX_COLUMNS = 3;

export type Align = "left" | "center" | "right";

/** Where an image gets its address: a library image, an https address, or the logo of the theme in effect. */
export type ImageSrc = { url: string } | { asset: string } | { theme: "logo" };

/** A condition the server evaluates: a node of the segment AST. */
export type VisibleIf = ConditionNode;

export interface HeadingBlock {
  type: "heading";
  id: string;
  level: 1 | 2 | 3;
  text: string;
  align?: Align;
  visible_if?: VisibleIf;
}
export interface TextBlock {
  type: "text";
  id: string;
  content: string;
  align?: Align;
  visible_if?: VisibleIf;
}
export interface ImageBlock {
  type: "image";
  id: string;
  src: ImageSrc;
  alt: string;
  href?: string;
  /** Leave the link without UTM parameters. Absent when off. */
  utm_off?: true;
  width?: number;
  visible_if?: VisibleIf;
}
export interface ButtonBlock {
  type: "button";
  id: string;
  label: string;
  url: string;
  /** Leave the link without UTM parameters. Absent when off. */
  utm_off?: true;
  align: Align;
  visible_if?: VisibleIf;
}
export interface DividerBlock {
  type: "divider";
  id: string;
  color?: string;
  visible_if?: VisibleIf;
}
export interface SpacerBlock {
  type: "spacer";
  id: string;
  height: number;
  visible_if?: VisibleIf;
}
export interface RawHtmlBlock {
  type: "raw_html";
  id: string;
  html: string;
  visible_if?: VisibleIf;
}
/** A reference to a snippet. The editor shows it as a read-only card; its blocks are edited in the snippet. */
export interface SnippetBlock {
  type: "snippet";
  id: string;
  snippet_id: string;
  visible_if?: VisibleIf;
}

export type Block = HeadingBlock | TextBlock | ImageBlock | ButtonBlock | DividerBlock | SpacerBlock | RawHtmlBlock | SnippetBlock;
export type BlockType = Block["type"];

export interface Section {
  id: string;
  background_color?: string;
  padding?: string;
  visible_if?: VisibleIf;
  columns: Block[][];
}

export interface DocumentStyle {
  background_color: string;
  content_width: number;
  font_family: string;
  text_color: string;
  link_color: string;
  button_color: string;
  button_text_color: string;
}

/** The theme a document follows: the workspace default, none (its own style), or a named theme. */
export type ThemeRef = "default" | "none" | { id: string };
/** Whether the theme's footer is the last item of the email. */
export type FooterRef = "theme" | "none";

export interface BlocksContent {
  subject: string;
  preheader: string;
  sender?: string;
  lang?: string;
  category: "marketing" | "transactional";
  theme?: ThemeRef;
  footer?: FooterRef;
  settings: DocumentStyle;
  sections: Section[];
}

export const DEFAULT_STYLE: DocumentStyle = {
  background_color: "#ffffff",
  content_width: 600,
  font_family: "arial",
  text_color: "#222222",
  link_color: "#1a5fb4",
  button_color: "#1a5fb4",
  button_text_color: "#ffffff",
};

/** The web-safe stacks the server accepts, by name. */
export const FONT_STACKS: readonly { id: string; label: string }[] = [
  { id: "arial", label: "Arial" },
  { id: "helvetica", label: "Helvetica" },
  { id: "verdana", label: "Verdana" },
  { id: "tahoma", label: "Tahoma" },
  { id: "trebuchet", label: "Trebuchet MS" },
  { id: "georgia", label: "Georgia" },
  { id: "times", label: "Times New Roman" },
  { id: "courier", label: "Courier New" },
];

/** The blocks the palette adds by a click or a drag. A snippet block is added through its picker. */
export type PaletteType = Exclude<BlockType, "snippet">;

export const BLOCK_TYPES: readonly { type: PaletteType; label: string }[] = [
  { type: "heading", label: "Heading" },
  { type: "text", label: "Text" },
  { type: "image", label: "Image" },
  { type: "button", label: "Button" },
  { type: "divider", label: "Divider" },
  { type: "spacer", label: "Spacer" },
  { type: "raw_html", label: "Raw HTML" },
];

export function blockLabel(type: BlockType): string {
  return type === "snippet" ? "Snippet" : (BLOCK_TYPES.find((entry) => entry.type === type)?.label ?? type);
}

export function emptyContent(category: "marketing" | "transactional"): BlocksContent {
  return { subject: "", preheader: "", category, theme: "default", footer: "theme", settings: { ...DEFAULT_STYLE }, sections: [] };
}

/** The object without `key`. */
export function omit<T extends object>(value: T, key: string): T {
  return Object.fromEntries(Object.entries(value).filter(([name]) => name !== key)) as T;
}

function record(value: unknown): Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value) ? (value as Record<string, unknown>) : {};
}

/** A schema 1 image has its address as a string in `src`; the editor works on the schema 2 shape. */
function readBlock(block: Block): Block {
  const raw = block as unknown as Record<string, unknown>;
  const read = raw["visible_if"] === null ? omit(raw, "visible_if") : raw;
  if (read["type"] === "image" && typeof read["src"] === "string") return { ...read, src: { url: read["src"] } } as unknown as Block;
  return read as unknown as Block;
}

const CONDITION_KEYS = ["all", "any", "not", "trait", "event", "list", "segment", "consent", "messages", "identifier"];

/** Whether the condition editor can show `value`: a plain object with exactly one known operator key, checked down the tree. */
export function isEditableCondition(value: unknown): value is VisibleIf {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return false;
  const keys = Object.keys(value).filter((key) => CONDITION_KEYS.includes(key));
  const node = value as Record<string, unknown>;
  const key = keys[0];
  if (keys.length !== 1 || key === undefined) return false;
  const inner = node[key];
  switch (key) {
    case "all":
    case "any":
      return Array.isArray(inner) && inner.every(isEditableCondition);
    case "not":
      return isEditableCondition(inner);
    case "trait":
      return typeof inner === "string" && typeof node["op"] === "string";
    case "event":
      return (
        typeof inner === "string" &&
        (node["where"] === undefined || (Array.isArray(node["where"]) && node["where"].every(isPropertyFilter))) &&
        (node["agg"] === undefined || isAggregate(node["agg"])) &&
        (node["window"] === undefined || isWindow(node["window"]))
      );
    case "consent":
      return isPlainObject(inner) && typeof inner["status"] === "string" && isOptionalText(inner["channel"]) && isOptionalText(inner["topic"]);
    case "messages":
      return (
        isPlainObject(inner) &&
        typeof inner["channel"] === "string" &&
        isAggregate(inner["agg"]) &&
        (inner["dimensions"] === undefined || (isPlainObject(inner["dimensions"]) && Object.values(inner["dimensions"]).every((entry) => typeof entry === "string"))) &&
        (inner["window"] === undefined || isWindow(inner["window"]))
      );
    default:
      return typeof inner === "string";
  }
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isOptionalText(value: unknown): boolean {
  return value === undefined || typeof value === "string";
}

function isPropertyFilter(value: unknown): boolean {
  return isPlainObject(value) && typeof value["prop"] === "string" && typeof value["op"] === "string";
}

function isAggregate(value: unknown): boolean {
  return isPlainObject(value) && AGG_FNS.some((fn) => fn === value["fn"]) && typeof value["op"] === "string" && (value["prop"] === undefined || typeof value["prop"] === "string");
}

function isWindow(value: unknown): boolean {
  if (!isPlainObject(value)) return false;
  return typeof value["last"] === "string" || typeof value["since_trait"] === "string" || (Array.isArray(value["between"]) && value["between"].every((end) => typeof end === "string"));
}

/** Reads the `blocks` object of a stored document. A value that is not shaped like the document gives what can be read; the server decides what is valid. */
export function readContent(value: unknown): BlocksContent {
  const raw = record(value);
  const settings = record(raw["settings"]);
  const style: Record<string, unknown> = { ...DEFAULT_STYLE };
  for (const key of Object.keys(DEFAULT_STYLE) as (keyof DocumentStyle)[]) {
    const entry = settings[key];
    if (key === "content_width" ? typeof entry === "number" : typeof entry === "string") style[key] = entry;
  }
  const sections = Array.isArray(raw["sections"]) ? (raw["sections"] as unknown[]) : [];
  const content: BlocksContent = {
    ...raw,
    subject: typeof raw["subject"] === "string" ? raw["subject"] : "",
    preheader: typeof raw["preheader"] === "string" ? raw["preheader"] : "",
    category: raw["category"] === "transactional" ? "transactional" : "marketing",
    settings: { ...settings, ...style } as unknown as DocumentStyle,
    sections: sections.map((section) => {
      const entry = record(section);
      const columns = Array.isArray(entry["columns"]) ? (entry["columns"] as unknown[]) : [];
      const read: Section = {
        ...entry,
        id: typeof entry["id"] === "string" ? entry["id"] : "",
        columns: columns.map((column) =>
          Array.isArray(column) ? ((column as unknown[]).filter((block) => typeof block === "object" && block !== null) as Block[]).map(readBlock) : [],
        ),
      };
      if (typeof entry["background_color"] === "string") read.background_color = entry["background_color"];
      if (typeof entry["padding"] === "string") read.padding = entry["padding"];
      if (entry["visible_if"] == null) delete read.visible_if;
      return read;
    }),
  };
  if (typeof raw["sender"] === "string") content.sender = raw["sender"];
  if (typeof raw["lang"] === "string") content.lang = raw["lang"];
  if (raw["theme"] !== undefined) content.theme = raw["theme"] as ThemeRef;
  if (raw["footer"] !== undefined) content.footer = raw["footer"] as FooterRef;
  return content;
}

/** Every id in use, for new ids that never collide. */
export function usedIds(content: BlocksContent): Set<string> {
  const ids = new Set<string>();
  for (const section of content.sections) {
    ids.add(section.id);
    for (const column of section.columns) for (const block of column) ids.add(block.id);
  }
  return ids;
}

export function freshId(prefix: string, content: BlocksContent): string {
  const used = usedIds(content);
  let n = 1;
  while (used.has(`${prefix}-${n}`)) n += 1;
  return `${prefix}-${n}`;
}

export function blockCount(content: BlocksContent): number {
  return content.sections.reduce((sum, section) => sum + section.columns.reduce((inner, column) => inner + column.length, 0), 0);
}

/** A new block with the fields the v1 document needs and nothing else. */
export function newBlock(type: PaletteType, content: BlocksContent): Block {
  const id = freshId(type.replace("_", "-"), content);
  switch (type) {
    case "heading":
      return { type, id, level: 2, text: "Your heading" };
    case "text":
      return { type, id, content: "<p>Write your text here.</p>" };
    case "image":
      return { type, id, src: { url: "" }, alt: "" };
    case "button":
      return { type, id, label: "Button", url: "", align: "left" };
    case "divider":
      return { type, id };
    case "spacer":
      return { type, id, height: 20 };
    case "raw_html":
      return { type, id, html: "" };
  }
}

export function newSection(columns: number, content: BlocksContent): Section {
  return { id: freshId("section", content), columns: Array.from({ length: columns }, () => []) };
}

export interface BlockPath {
  section: number;
  column: number;
  index: number;
}

/** Where the block with `id` is, or null. */
export function findBlock(content: BlocksContent, id: string): BlockPath | null {
  for (const [section, entry] of content.sections.entries()) {
    for (const [column, blocks] of entry.columns.entries()) {
      const index = blocks.findIndex((block) => block.id === id);
      if (index >= 0) return { section, column, index };
    }
  }
  return null;
}

export function blockAt(content: BlocksContent, path: BlockPath): Block | undefined {
  return content.sections[path.section]?.columns[path.column]?.[path.index];
}

export function findById(content: BlocksContent, id: string): Block | undefined {
  const path = findBlock(content, id);
  return path ? blockAt(content, path) : undefined;
}

/** Replaces the block with `id`. */
export function updateBlock(content: BlocksContent, id: string, change: (block: Block) => Block): BlocksContent {
  return {
    ...content,
    sections: content.sections.map((section) => ({
      ...section,
      columns: section.columns.map((column) => column.map((block) => (block.id === id ? change(block) : block))),
    })),
  };
}

export function updateSection(content: BlocksContent, id: string, change: (section: Section) => Section): BlocksContent {
  return { ...content, sections: content.sections.map((section) => (section.id === id ? change(section) : section)) };
}

function insertAt<T>(list: readonly T[], index: number, item: T): T[] {
  const at = Math.max(0, Math.min(index, list.length));
  return [...list.slice(0, at), item, ...list.slice(at)];
}

export function insertSection(content: BlocksContent, section: Section, index: number): BlocksContent {
  return { ...content, sections: insertAt(content.sections, index, section) };
}

export function insertBlock(content: BlocksContent, block: Block, target: BlockPath): BlocksContent {
  return {
    ...content,
    sections: content.sections.map((section, s) =>
      s === target.section
        ? { ...section, columns: section.columns.map((column, c) => (c === target.column ? insertAt(column, target.index, block) : column)) }
        : section,
    ),
  };
}

export function removeBlock(content: BlocksContent, id: string): BlocksContent {
  return {
    ...content,
    sections: content.sections.map((section) => ({ ...section, columns: section.columns.map((column) => column.filter((block) => block.id !== id)) })),
  };
}

export function removeSection(content: BlocksContent, id: string): BlocksContent {
  return { ...content, sections: content.sections.filter((section) => section.id !== id) };
}

/** What is being dragged. A section is a root item and a block lives in a column; nothing nests a section. */
export type Drag =
  | { kind: "section"; id: string }
  | { kind: "block"; id: string }
  | { kind: "new-section"; columns: number }
  | { kind: "new-block"; type: PaletteType };

/** Where it is dropped: between the sections, or at a place in a column. */
export type Drop = { kind: "root"; index: number } | ({ kind: "column" } & BlockPath);

/** Whether `drag` may be dropped on `drop`: a section only at the root, a block only in a column. */
export function canDrop(content: BlocksContent, drag: Drag, drop: Drop): boolean {
  const isSection = drag.kind === "section" || drag.kind === "new-section";
  if (isSection !== (drop.kind === "root")) return false;
  if (drop.kind === "root") {
    if (drag.kind === "new-section" && content.sections.length >= MAX_SECTIONS) return false;
    return drop.index >= 0 && drop.index <= content.sections.length;
  }
  if (drag.kind === "new-block" && blockCount(content) >= MAX_BLOCKS) return false;
  return content.sections[drop.section]?.columns[drop.column] !== undefined;
}

/** The document after the drop, or null when the drop is not allowed. */
export function applyDrop(content: BlocksContent, drag: Drag, drop: Drop): BlocksContent | null {
  if (!canDrop(content, drag, drop)) return null;
  if (drag.kind === "new-section" && drop.kind === "root") {
    return insertSection(content, newSection(drag.columns, content), drop.index);
  }
  if (drag.kind === "new-block" && drop.kind === "column") {
    return insertBlock(content, newBlock(drag.type, content), drop);
  }
  if (drag.kind === "section" && drop.kind === "root") {
    const from = content.sections.findIndex((section) => section.id === drag.id);
    const moving = content.sections[from];
    if (!moving) return null;
    const rest = content.sections.filter((section) => section.id !== drag.id);
    const to = drop.index > from ? drop.index - 1 : drop.index;
    return { ...content, sections: insertAt(rest, to, moving) };
  }
  if (drag.kind === "block" && drop.kind === "column") {
    const from = findBlock(content, drag.id);
    const moving = from ? blockAt(content, from) : undefined;
    if (!from || !moving) return null;
    const sameColumn = from.section === drop.section && from.column === drop.column;
    const index = sameColumn && drop.index > from.index ? drop.index - 1 : drop.index;
    return insertBlock(removeBlock(content, drag.id), moving, { section: drop.section, column: drop.column, index });
  }
  return null;
}

/** All the columns in reading order, for moving a block across them. */
function columnOrder(content: BlocksContent): { section: number; column: number }[] {
  return content.sections.flatMap((section, s) => section.columns.map((_, c) => ({ section: s, column: c })));
}

/** The keyboard move of a block: one place up or down, crossing into the neighbouring column at an edge. Null when it cannot move. */
export function moveBlock(content: BlocksContent, id: string, direction: "up" | "down"): BlocksContent | null {
  const from = findBlock(content, id);
  const moving = from ? blockAt(content, from) : undefined;
  if (!from || !moving) return null;
  const column = content.sections[from.section]?.columns[from.column] ?? [];
  const without = removeBlock(content, id);
  if (direction === "up" && from.index > 0) return insertBlock(without, moving, { ...from, index: from.index - 1 });
  if (direction === "down" && from.index < column.length - 1) return insertBlock(without, moving, { ...from, index: from.index + 1 });
  const order = columnOrder(content);
  const at = order.findIndex((entry) => entry.section === from.section && entry.column === from.column);
  const next = order[at + (direction === "up" ? -1 : 1)];
  if (!next) return null;
  const target = without.sections[next.section]?.columns[next.column] ?? [];
  return insertBlock(without, moving, { ...next, index: direction === "up" ? target.length : 0 });
}

export function moveSection(content: BlocksContent, id: string, direction: "up" | "down"): BlocksContent | null {
  const from = content.sections.findIndex((section) => section.id === id);
  const to = direction === "up" ? from - 1 : from + 1;
  const moving = content.sections[from];
  if (!moving || to < 0 || to >= content.sections.length) return null;
  const rest = content.sections.filter((section) => section.id !== id);
  return { ...content, sections: insertAt(rest, to, moving) };
}

/** Where a new section goes: after the selected section, or after the section of the selected block, or at the end. */
export function sectionInsertIndex(content: BlocksContent, selected: string | null): number {
  if (selected === null) return content.sections.length;
  const own = content.sections.findIndex((section) => section.id === selected);
  if (own >= 0) return own + 1;
  const path = findBlock(content, selected);
  return path ? path.section + 1 : content.sections.length;
}

/**
 * Where a new block goes: after the selected block, at the end of the first column of the selected
 * section, or at the end of the first column of the last section. With no section, the document
 * gains one single-column section first.
 */
export function placeNewBlock(content: BlocksContent, selected: string | null): { base: BlocksContent; place: BlockPath } {
  if (content.sections.length === 0) {
    return { base: insertSection(content, newSection(1, content), 0), place: { section: 0, column: 0, index: 0 } };
  }
  const path = selected === null ? null : findBlock(content, selected);
  if (path) return { base: content, place: { ...path, index: path.index + 1 } };
  const own = selected === null ? -1 : content.sections.findIndex((section) => section.id === selected);
  const section = own >= 0 ? own : content.sections.length - 1;
  return { base: content, place: { section, column: 0, index: content.sections[section]?.columns[0]?.length ?? 0 } };
}

/** The id in a block or section pointer `/blocks/block/<id>[/field]` or `/blocks/sections/<id>[/field]`, or null for any other place. */
export function issueBlockId(at: string): string | null {
  const prefix = ["/blocks/block/", "/blocks/sections/"].find((entry) => at.startsWith(entry));
  return prefix === undefined ? null : (at.slice(prefix.length).split("/")[0] ?? null);
}

/** The issues that point at the block or section `id`, under either pointer form. */
export function issuesOf<T extends { at: string }>(issues: readonly T[], id: string): T[] {
  return issues.filter((issue) => issueBlockId(issue.at) === id);
}

/** `value` with `visible_if` set, or removed when `condition` is undefined. */
export function withVisibleIf<T extends { visible_if?: VisibleIf }>(value: T, condition: VisibleIf | undefined): T {
  return condition === undefined ? omit(value, "visible_if") : { ...value, visible_if: condition };
}

/** A new snippet block for the published snippet `snippetId`. */
export function newSnippetBlock(snippetId: string, content: BlocksContent): SnippetBlock {
  return { type: "snippet", id: freshId("snippet", content), snippet_id: snippetId };
}
