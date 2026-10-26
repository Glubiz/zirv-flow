import type { DragEvent } from "react";
import { Link } from "react-router";

import { Button, EmptyState, MenuButton, Pill } from "../../../ui";
import { describeCondition } from "../../segments";
import { FieldIssues } from "../IssueList";
import type { TemplateIssue } from "../model";
import { blockLabel, canDrop, issuesOf, moveBlock, type Block, type BlocksContent, type Drag, type Drop, type Section, type VisibleIf } from "./document";

/** What the canvas knows of the snippets a document holds, by id. */
export type SnippetNames = ReadonlyMap<string, { name: string; version: number | null }>;

/** The theme footer as the last, locked item of a document. */
export interface FooterView {
  /** The theme it comes from. */
  theme: string;
  /** Whether it was taken out of this email. */
  removed: boolean;
  /** Marketing email needs an unsubscribe link; without the footer the email must carry its own. */
  marketing: boolean;
  onRemove: () => void;
  onRestore: () => void;
}

interface Props {
  content: BlocksContent;
  selected: string | null;
  onSelect: (id: string) => void;
  issues: readonly TemplateIssue[];
  readOnly: boolean;
  drag: Drag | null;
  onDragChange: (drag: Drag | null) => void;
  onDrop: (drag: Drag, drop: Drop) => void;
  onMove: (kind: "block" | "section", id: string, direction: "up" | "down") => void;
  onRemove: (kind: "block" | "section", id: string) => void;
  /** One column of blocks with no section chrome: how a snippet is laid out. */
  flat?: boolean;
  /** The workspace, for the link that opens a snippet. Absent where blocks cannot be snippets. */
  workspaceId?: string;
  snippets?: SnippetNames;
  /** The theme footer, when a theme is in effect. */
  footer?: FooterView;
}

/** The badge a conditional block or section carries: short, with the whole condition on hover. */
function ConditionBadge({ condition }: { condition: VisibleIf | undefined }) {
  if (condition === undefined) return null;
  return (
    <span className="be-badge" title={describeCondition(condition)}>
      Only when…
    </span>
  );
}

/** A line of words that stands for a block on the canvas. The block is never rendered here; the preview is the server's. */
function summary(block: Block, snippets: SnippetNames | undefined): string {
  switch (block.type) {
    case "heading":
      return block.text === "" ? "Empty heading" : block.text;
    case "text": {
      const text = block.content.replace(/<[^>]*>/g, " ").replace(/\s+/g, " ").trim();
      return text === "" ? "Empty text" : text;
    }
    case "image": {
      const name = block.alt === "" ? "Image" : block.alt;
      if ("url" in block.src) return block.src.url === "" ? "Image with no address yet" : `${name}: ${block.src.url}`;
      return "asset" in block.src ? `${name}: an uploaded image` : `${name}: the theme logo`;
    }
    case "button":
      return `${block.label === "" ? "Button" : block.label} to ${block.url === "" ? "no address yet" : block.url}`;
    case "divider":
      return "A dividing line";
    case "spacer":
      return `${block.height} pixels of space`;
    case "raw_html":
      return block.html === "" ? "Empty HTML" : `${block.html.length} characters of your own HTML`;
    case "snippet":
      return snippets?.get(block.snippet_id)?.name ?? `Snippet ${block.snippet_id} (locked)`;
  }
}

function startDrag(event: DragEvent, drag: Drag, set: (drag: Drag | null) => void) {
  set(drag);
  const transfer = event.dataTransfer as DataTransfer | null;
  if (!transfer) return;
  transfer.effectAllowed = "move";
  // Firefox does not start a drag without data.
  transfer.setData("text/plain", "zirv-block");
}

export function BlockCanvas(props: Props) {
  const { content, readOnly, drag, onDragChange, onDrop } = props;
  const ordinals = new Map<string, number>();
  let count = 0;
  for (const section of content.sections) for (const column of section.columns) for (const block of column) ordinals.set(block.id, (count += 1));

  function slot(drop: Drop, label: string) {
    const allowed = drag !== null && canDrop(content, drag, drop);
    const key = drop.kind === "root" ? `root-${drop.index}` : `column-${drop.section}-${drop.column}-${drop.index}`;
    return (
      <div
        key={key}
        className={allowed ? "be-slot be-slot-open" : "be-slot"}
        data-testid={`slot-${key}`}
        data-drop-label={label}
        onDragOver={(event) => {
          if (allowed) {
            event.preventDefault();
            const transfer = event.dataTransfer as DataTransfer | null;
            if (transfer) transfer.dropEffect = "move";
          }
        }}
        onDrop={(event) => {
          event.preventDefault();
          if (drag && allowed) onDrop(drag, drop);
          onDragChange(null);
        }}
      />
    );
  }

  function handle(what: string, payload: Drag) {
    return (
      <span
        className="be-handle"
        role="img"
        aria-label={`Drag handle for ${what}. Drag it with the mouse, or use the move up and move down buttons.`}
        draggable={!readOnly}
        onDragStart={(event) => startDrag(event, payload, onDragChange)}
        onDragEnd={() => onDragChange(null)}
      >
        <span aria-hidden="true">::</span>
      </span>
    );
  }

  function blockView(block: Block, siblings: number, index: number) {
    const label = blockLabel(block.type);
    const number = ordinals.get(block.id) ?? 0;
    const name = `${label} block ${number}`;
    const own = issuesOf(props.issues, block.id);
    const held = block.type === "snippet" ? props.snippets?.get(block.snippet_id) : undefined;
    const chosen = props.selected === block.id;
    return (
      <div
        role="group"
        aria-label={name}
        className={[chosen ? "be-block be-block-on" : "be-block", block.type === "snippet" ? "be-snippet" : ""].filter(Boolean).join(" ")}
        data-block-id={block.id}
        tabIndex={0}
        onFocus={() => props.onSelect(block.id)}
        onClick={() => props.onSelect(block.id)}
        onKeyDown={(event) => {
          if (readOnly || !event.altKey || event.target !== event.currentTarget) return;
          if (event.key === "ArrowUp" || event.key === "ArrowDown") {
            event.preventDefault();
            props.onMove("block", block.id, event.key === "ArrowUp" ? "up" : "down");
          }
        }}
      >
        <div className="be-bar">
          {readOnly ? null : handle(name, { kind: "block", id: block.id })}
          {block.type === "snippet" ? null : <span className="strong">{label}</span>}
          {block.type === "snippet" ? <Pill tone="neutral">Snippet</Pill> : null}
          <ConditionBadge condition={block.visible_if} />
          {chosen ? <span className="faint">selected</span> : null}
          {readOnly ? null : (
            <span className="be-actions">
              <Button data-move="up" aria-label={`Move ${name} up`} onClick={() => props.onMove("block", block.id, "up")} disabled={moveBlock(content, block.id, "up") === null}>
                Up
              </Button>
              <Button data-move="down" aria-label={`Move ${name} down`} onClick={() => props.onMove("block", block.id, "down")} disabled={moveBlock(content, block.id, "down") === null}>
                Down
              </Button>
              <Button aria-label={`Remove ${name}`} onClick={() => props.onRemove("block", block.id)}>
                Remove
              </Button>
            </span>
          )}
        </div>
        <p className="be-summary">
          {summary(block, props.snippets)}
          {held && held.version !== null ? <span className="faint"> · version {held.version}</span> : null}
          {block.type === "snippet" && held && props.workspaceId ? (
            <>
              {" "}
              <Link to={`/w/${props.workspaceId}/snippets/${block.snippet_id}`} target="_blank" rel="noreferrer" onClick={(event) => event.stopPropagation()}>
                Open snippet
              </Link>
            </>
          ) : null}
        </p>
        <FieldIssues issues={own} />
        <span hidden data-position={`${index + 1}/${siblings}`} />
      </div>
    );
  }

  function sectionView(section: Section, index: number) {
    const name = `Section ${index + 1}`;
    const chosen = props.selected === section.id;
    return (
      <div data-section-id={section.id} role="group" aria-label={`${name}, ${section.columns.length} ${section.columns.length === 1 ? "column" : "columns"}`} className={chosen ? "be-section be-section-on" : "be-section"}>
        <div className="be-bar">
          {readOnly ? null : handle(name, { kind: "section", id: section.id })}
          <button type="button" data-section-settings className="link-button strong" aria-pressed={chosen} onClick={() => props.onSelect(section.id)}>
            {name} settings
          </button>
          <span className="faint">{section.columns.length === 1 ? "1 column" : `${section.columns.length} columns`}</span>
          <ConditionBadge condition={section.visible_if} />
          {readOnly ? null : (
            <span className="be-actions">
              <Button data-move="up" aria-label={`Move ${name} up`} onClick={() => props.onMove("section", section.id, "up")} disabled={index === 0}>
                Up
              </Button>
              <Button data-move="down" aria-label={`Move ${name} down`} onClick={() => props.onMove("section", section.id, "down")} disabled={index === content.sections.length - 1}>
                Down
              </Button>
              <Button aria-label={`Remove ${name}`} onClick={() => props.onRemove("section", section.id)}>
                Remove
              </Button>
            </span>
          )}
        </div>
        <FieldIssues issues={issuesOf(props.issues, section.id)} />
        <div className="be-columns" style={{ gridTemplateColumns: `repeat(${section.columns.length}, minmax(0, 1fr))` }}>
          {section.columns.map((column, c) => (
            <div key={c} role="group" aria-label={`${name}, column ${c + 1}`} className="be-column">
              {slot({ kind: "column", section: index, column: c, index: 0 }, `Start of ${name}, column ${c + 1}`)}
              {column.length === 0 ? <p className="faint be-empty-column">Empty column. Add a block from the palette.</p> : null}
              {column.map((block, b) => (
                <div key={block.id}>
                  {blockView(block, column.length, b)}
                  {slot({ kind: "column", section: index, column: c, index: b + 1 }, `After block ${ordinals.get(block.id) ?? 0}`)}
                </div>
              ))}
            </div>
          ))}
        </div>
      </div>
    );
  }

  function footerView(footer: FooterView) {
    const menu = readOnly ? null : (
      <span className="be-actions">
        <MenuButton
          label="Theme footer actions"
          buttonClassName="btn"
          buttonContent={<span aria-hidden="true">⋯</span>}
          entries={footer.removed ? [{ key: "restore", label: "Restore", onSelect: footer.onRestore }] : [{ key: "remove", label: "Remove", onSelect: footer.onRemove }]}
        />
      </span>
    );
    return (
      <div className={footer.removed ? "be-footer be-footer-off" : "be-footer"} role="group" aria-label="Theme footer">
        <div className="be-bar">
          <span className="be-lock" role="img" aria-label="Locked: it stays the last item">
            <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true" fill="none" stroke="currentColor" strokeWidth="1.5">
              <rect x="3" y="7" width="10" height="7" rx="1.5" />
              <path d="M5.5 7V5a2.5 2.5 0 0 1 5 0v2" />
            </svg>
          </span>
          <span className="strong">Theme footer · from {footer.theme}</span>
          {footer.removed ? <span className="faint">removed from this email</span> : null}
          {menu}
        </div>
        {footer.removed && footer.marketing ? (
          <p className="notice" role="status">
            Without the theme footer, a marketing email needs its own unsubscribe link.
          </p>
        ) : null}
      </div>
    );
  }

  if (props.flat) {
    const column = content.sections[0]?.columns[0] ?? [];
    return (
      <div className={drag ? "be-canvas be-dragging" : "be-canvas"} role="group" aria-label="Canvas">
        {column.length === 0 ? <EmptyState title="No blocks yet">Add a block from the palette, or drag one onto the canvas.</EmptyState> : null}
        {slot({ kind: "column", section: 0, column: 0, index: 0 }, "Start of the snippet")}
        {column.map((block, b) => (
          <div key={block.id}>
            {blockView(block, column.length, b)}
            {slot({ kind: "column", section: 0, column: 0, index: b + 1 }, `After block ${ordinals.get(block.id) ?? 0}`)}
          </div>
        ))}
      </div>
    );
  }
  if (content.sections.length === 0) {
    return (
      <div className="be-canvas" aria-label="Canvas">
        <EmptyState title="No blocks yet">Add a section from the palette, then add blocks to its columns. You can also drag from the palette onto the canvas.</EmptyState>
        {slot({ kind: "root", index: 0 }, "Start of the document")}
        {props.footer ? footerView(props.footer) : null}
      </div>
    );
  }
  return (
    <div className={drag ? "be-canvas be-dragging" : "be-canvas"} role="group" aria-label="Canvas">
      {slot({ kind: "root", index: 0 }, "Start of the document")}
      {content.sections.map((section, index) => (
        <div key={section.id}>
          {sectionView(section, index)}
          {slot({ kind: "root", index: index + 1 }, `After section ${index + 1}`)}
        </div>
      ))}
      {props.footer ? footerView(props.footer) : null}
    </div>
  );
}
