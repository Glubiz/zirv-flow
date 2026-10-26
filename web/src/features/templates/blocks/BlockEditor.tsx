import { useEffect, useRef, useState, type DragEvent } from "react";

import { Button, MenuButton } from "../../../ui";
import { IssueList } from "../IssueList";
import { MergeTagPicker } from "../MergeTagPicker";
import { TabBar } from "../TabBar";
import type { Editable, Template, TemplateIssue } from "../model";
import { BlockCanvas } from "./BlockCanvas";
import { BlockSettings, SectionSettings } from "./BlockSettings";
import { DocumentStyleForm, MessageForm } from "./DocumentPanel";
import {
  MAX_BLOCKS,
  MAX_SECTIONS,
  applyDrop,
  blockCount,
  blockLabel,
  findById,
  issueBlockId,
  insertBlock,
  insertSection,
  moveBlock,
  moveSection,
  newBlock,
  newSection,
  placeNewBlock,
  sectionInsertIndex,
  removeBlock,
  removeSection,
  updateBlock,
  updateSection,
  BLOCK_TYPES,
  type PaletteType,
  type BlocksContent,
  type Drag,
  type Drop,
} from "./document";
import type { InsertTarget } from "./insertTarget";

type Blocks = Extract<Editable, { kind: "blocks" }>;
type PanelTab = "block" | "style" | "message";

const PANEL_TABS: readonly { id: PanelTab; label: string }[] = [
  { id: "block", label: "Block settings" },
  { id: "style", label: "Document style" },
  { id: "message", label: "Message" },
];

interface Props {
  workspaceId: string;
  doc: Blocks;
  onChange: (next: Editable) => void;
  readOnly: boolean;
  issues: readonly TemplateIssue[];
  checked: boolean;
  /** The template, for its UTM override in the Message tab. */
  template: Template;
  onTemplate: (next: Template) => void;
  /** Opens the one-way conversion to MJML code. Absent when the caller cannot convert. */
  onConvert?: () => void;
}

/** The block editor pane: palette, canvas, a docked settings panel, the validation list and the merge-tag picker. */
export function BlockEditor({ workspaceId, doc, onChange, readOnly, issues, checked, template, onTemplate, onConvert }: Props) {
  const content = doc.content;
  const locked = readOnly || doc.newer;
  const [selected, setSelected] = useState<string | null>(null);
  const [tab, setTab] = useState<PanelTab>("block");
  const [drag, setDrag] = useState<Drag | null>(null);
  const [target, setTarget] = useState<InsertTarget | null>(null);
  const [status, setStatus] = useState("");
  const pane = useRef<HTMLElement>(null);
  /** Selectors to try, in order, for the control that takes focus after the next render. */
  const refocus = useRef<string[]>([]);

  // A move across columns remounts the block, and a remove takes the focused control away; either
  // would drop focus to the page, so focus is put back on the block's control, or its neighbour.
  useEffect(() => {
    const wanted = refocus.current;
    refocus.current = [];
    for (const selector of wanted) {
      const element = pane.current?.querySelector<HTMLElement>(selector);
      if (element && !(element instanceof HTMLButtonElement && element.disabled)) {
        element.focus();
        return;
      }
    }
  });

  function commit(next: BlocksContent) {
    onChange({ ...doc, content: next });
  }

  function addSection(columns: number) {
    if (content.sections.length >= MAX_SECTIONS) return;
    const section = newSection(columns, content);
    commit(insertSection(content, section, sectionInsertIndex(content, selected)));
    setSelected(section.id);
    setTab("block");
    setStatus(`Added a section with ${columns} ${columns === 1 ? "column" : "columns"}.`);
  }

  function addBlock(type: PaletteType) {
    if (blockCount(content) >= MAX_BLOCKS) return;
    const block = newBlock(type, content);
    const { base, place } = placeNewBlock(content, selected);
    commit(insertBlock(base, block, place));
    setSelected(block.id);
    setTab("block");
    setStatus(`Added a ${blockLabel(type).toLowerCase()} block.`);
  }

  /** Drag props for a palette button: the drag starts with data, as Firefox needs, and ends by clearing. */
  function paletteDrag(payload: Drag) {
    return {
      draggable: true,
      onDragStart: (event: DragEvent) => {
        setDrag(payload);
        (event.dataTransfer as DataTransfer | null)?.setData("text/plain", "zirv-block");
      },
      onDragEnd: () => setDrag(null),
    };
  }

  function drop(dragged: Drag, at: Drop) {
    const next = applyDrop(content, dragged, at);
    if (next) {
      commit(next);
      setStatus("Moved.");
    }
  }

  function move(kind: "block" | "section", id: string, direction: "up" | "down") {
    const next = kind === "block" ? moveBlock(content, id, direction) : moveSection(content, id, direction);
    if (next) {
      const own = kind === "block" ? `[data-block-id="${id}"]` : `[data-section-id="${id}"] > .be-bar`;
      refocus.current = [`${own} [data-move="${direction}"]`, kind === "block" ? own : `${own} [data-section-settings]`];
      commit(next);
      setStatus(`Moved ${direction}.`);
    }
  }

  function remove(kind: "block" | "section", id: string) {
    const order = kind === "block" ? content.sections.flatMap((entry) => entry.columns.flat().map((each) => each.id)) : content.sections.map((entry) => entry.id);
    const at = order.indexOf(id);
    const neighbour = order[at + 1] ?? order[at - 1];
    refocus.current =
      neighbour === undefined
        ? [".be-palette button"]
        : [kind === "block" ? `[data-block-id="${neighbour}"]` : `[data-section-id="${neighbour}"] > .be-bar [data-section-settings]`];
    commit(kind === "block" ? removeBlock(content, id) : removeSection(content, id));
    if (selected === id) setSelected(null);
    setStatus("Removed.");
  }

  const block = selected === null ? undefined : findById(content, selected);
  const section = selected === null ? undefined : content.sections.find((entry) => entry.id === selected);

  function go(where: string, issue: TemplateIssue) {
    const id = issueBlockId(issue.at);
    if (id !== null) {
      setSelected(id);
      setTab("block");
    } else if (where === "Document style") {
      setTab("style");
    } else if (where === "Message") {
      setTab("message");
    }
  }

  return (
    <section ref={pane} className="tp-pane be-pane" aria-label="Editor">
      <div className="be-head">
        <MenuButton
          label="Template mode"
          placement="below-start"
          buttonClassName="btn"
          buttonContent="Mode: Blocks"
          entries={onConvert ? [{ key: "convert", label: "Convert to MJML…", onSelect: onConvert }] : []}
          emptyText="Converting needs permission to edit."
        />
        <span className="faint">One-way: code cannot become blocks again.</span>
      </div>
      {doc.newer ? (
        <p className="notice" role="status">
          This template was written by a newer version of the editor. You can look at it and preview it, but not change it here.
        </p>
      ) : null}
      <p className="visually-hidden" role="status" aria-live="polite">
        {status}
      </p>
      {locked ? null : (
        <div className="be-palette" role="group" aria-label="Add a block">
          <div className="row">
            {[1, 2, 3].map((columns) => (
              <Button
                key={columns}
                aria-label={`Add a section with ${columns} ${columns === 1 ? "column" : "columns"}`}
                {...paletteDrag({ kind: "new-section", columns })}
                onClick={() => addSection(columns)}
                disabled={content.sections.length >= MAX_SECTIONS}
              >
                Section, {columns} {columns === 1 ? "col" : "cols"}
              </Button>
            ))}
          </div>
          <div className="row">
            {BLOCK_TYPES.map((entry) => (
              <Button
                key={entry.type}
                aria-label={`Add ${entry.label.toLowerCase()} block`}
                {...paletteDrag({ kind: "new-block", type: entry.type })}
                onClick={() => addBlock(entry.type)}
                disabled={blockCount(content) >= MAX_BLOCKS}
              >
                {entry.label}
              </Button>
            ))}
          </div>
          <p className="faint">Click to add after the selected block, or drag onto the canvas.</p>
        </div>
      )}
      <BlockCanvas
        content={content}
        selected={selected}
        onSelect={(id) => {
          setSelected(id);
          setTab("block");
        }}
        issues={issues}
        readOnly={locked}
        drag={drag}
        onDragChange={setDrag}
        onDrop={drop}
        onMove={move}
        onRemove={remove}
      />
      <div className="be-panel">
        <TabBar label="Settings panel" tabs={PANEL_TABS} tab={tab} onTab={setTab} />
        <div role="tabpanel" aria-label={PANEL_TABS.find((entry) => entry.id === tab)?.label} className="stack">
          {tab === "block" && block ? (
            <BlockSettings block={block} readOnly={locked} issues={issues} onTarget={setTarget} onChange={(next) => commit(updateBlock(content, block.id, () => next))} />
          ) : null}
          {tab === "block" && section ? (
            <SectionSettings section={section} readOnly={locked} issues={issues} onChange={(next) => commit(updateSection(content, section.id, () => next))} />
          ) : null}
          {tab === "block" && !block && !section ? <p className="faint">Select a block or a section to edit its settings.</p> : null}
          {tab === "style" ? <DocumentStyleForm content={content} readOnly={locked} issues={issues} onChange={commit} /> : null}
          {tab === "message" ? (
            <MessageForm workspaceId={workspaceId} content={content} readOnly={locked} issues={issues} onChange={commit} onTarget={setTarget} template={template} onTemplate={onTemplate} />
          ) : null}
        </div>
      </div>
      <IssueList issues={issues} checked={checked} onGo={go} />
      {locked ? null : (
        <MergeTagPicker
          workspaceId={workspaceId}
          disabled={target === null}
          target={target === null ? "a text field" : target.label}
          onInsert={(tag) => target?.insert(tag)}
        />
      )}
    </section>
  );
}
