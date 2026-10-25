import type { DragEvent } from "react";

import { Button, EmptyState } from "../../../ui";
import { FieldIssues } from "../IssueList";
import { issuesAt, type TemplateIssue } from "../model";
import { blockLabel, canDrop, type Block, type BlocksContent, type Drag, type Drop, type Section } from "./document";

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
}

/** A line of words that stands for a block on the canvas. The block is never rendered here; the preview is the server's. */
function summary(block: Block): string {
  switch (block.type) {
    case "heading":
      return block.text === "" ? "Empty heading" : block.text;
    case "text": {
      const text = block.content.replace(/<[^>]*>/g, " ").replace(/\s+/g, " ").trim();
      return text === "" ? "Empty text" : text;
    }
    case "image":
      return block.src === "" ? "Image with no address yet" : `${block.alt === "" ? "Image" : block.alt}: ${block.src}`;
    case "button":
      return `${block.label === "" ? "Button" : block.label} to ${block.url === "" ? "no address yet" : block.url}`;
    case "divider":
      return "A dividing line";
    case "spacer":
      return `${block.height} pixels of space`;
    case "raw_html":
      return block.html === "" ? "Empty HTML" : `${block.html.length} characters of your own HTML`;
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
    const own = issuesAt(props.issues, `/blocks/block/${block.id}`);
    const chosen = props.selected === block.id;
    return (
      <div
        role="group"
        aria-label={name}
        className={chosen ? "be-block be-block-on" : "be-block"}
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
          <span className="strong">{label}</span>
          {chosen ? <span className="faint">selected</span> : null}
          {readOnly ? null : (
            <span className="be-actions">
              <Button data-move="up" aria-label={`Move ${name} up`} onClick={() => props.onMove("block", block.id, "up")} disabled={number === 1}>
                Up
              </Button>
              <Button data-move="down" aria-label={`Move ${name} down`} onClick={() => props.onMove("block", block.id, "down")} disabled={number === count}>
                Down
              </Button>
              <Button aria-label={`Remove ${name}`} onClick={() => props.onRemove("block", block.id)}>
                Remove
              </Button>
            </span>
          )}
        </div>
        <p className="be-summary">{summary(block)}</p>
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
        <FieldIssues issues={issuesAt(props.issues, `/blocks/block/${section.id}`)} />
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

  if (content.sections.length === 0) {
    return (
      <div className="be-canvas" aria-label="Canvas">
        <EmptyState title="No blocks yet">Add a section from the palette, then add blocks to its columns. You can also drag from the palette onto the canvas.</EmptyState>
        {slot({ kind: "root", index: 0 }, "Start of the document")}
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
    </div>
  );
}
