import { useState } from "react";

import { Button, MenuButton } from "../../../ui";
import { IssueList } from "../IssueList";
import { MergeTagPicker } from "../MergeTagPicker";
import { TabBar } from "../TabBar";
import type { Editable, TemplateIssue } from "../model";
import { BlockCanvas } from "./BlockCanvas";
import { BlockSettings, SectionSettings } from "./BlockSettings";
import { DocumentStyleForm, MessageForm } from "./DocumentPanel";
import {
  MAX_BLOCKS,
  MAX_SECTIONS,
  applyDrop,
  blockCount,
  findBlock,
  findById,
  issueBlockId,
  insertBlock,
  insertSection,
  moveBlock,
  moveSection,
  newBlock,
  newSection,
  removeBlock,
  removeSection,
  updateBlock,
  updateSection,
  BLOCK_TYPES,
  type BlockType,
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
  canAdmin: boolean;
  issues: readonly TemplateIssue[];
  checked: boolean;
  /** Opens the one-way conversion to MJML code. Absent when the caller cannot convert. */
  onConvert?: () => void;
}

/** The block editor pane: palette, canvas, a docked settings panel, the validation list and the merge-tag picker. */
export function BlockEditor({ workspaceId, doc, onChange, readOnly, canAdmin, issues, checked, onConvert }: Props) {
  const content = doc.content;
  const locked = readOnly || doc.newer;
  const [selected, setSelected] = useState<string | null>(null);
  const [tab, setTab] = useState<PanelTab>("block");
  const [drag, setDrag] = useState<Drag | null>(null);
  const [target, setTarget] = useState<InsertTarget | null>(null);
  const [status, setStatus] = useState("");

  function commit(next: BlocksContent) {
    onChange({ ...doc, content: next });
  }

  /** Where a new section goes: after the selected section, or after the section of the selected block, or at the end. */
  function sectionIndex(): number {
    if (selected === null) return content.sections.length;
    const own = content.sections.findIndex((section) => section.id === selected);
    if (own >= 0) return own + 1;
    const path = findBlock(content, selected);
    return path ? path.section + 1 : content.sections.length;
  }

  function addSection(columns: number) {
    if (content.sections.length >= MAX_SECTIONS) return;
    const section = newSection(columns, content);
    commit(insertSection(content, section, sectionIndex()));
    setSelected(section.id);
    setTab("block");
    setStatus(`Added a section with ${columns} ${columns === 1 ? "column" : "columns"}.`);
  }

  function addBlock(type: BlockType) {
    if (blockCount(content) >= MAX_BLOCKS) return;
    const block = newBlock(type, content);
    let base = content;
    let place = { section: 0, column: 0, index: 0 };
    if (content.sections.length === 0) {
      base = insertSection(content, newSection(1, content), 0);
    } else {
      const path = selected === null ? null : findBlock(content, selected);
      const own = selected === null ? -1 : content.sections.findIndex((section) => section.id === selected);
      const last = content.sections.length - 1;
      if (path) place = { ...path, index: path.index + 1 };
      else if (own >= 0) place = { section: own, column: 0, index: content.sections[own]?.columns[0]?.length ?? 0 };
      else place = { section: last, column: 0, index: content.sections[last]?.columns[0]?.length ?? 0 };
    }
    commit(insertBlock(base, block, place));
    setSelected(block.id);
    setTab("block");
    setStatus(`Added a ${BLOCK_TYPES.find((entry) => entry.type === type)?.label.toLowerCase() ?? type} block.`);
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
      commit(next);
      setStatus(`Moved ${direction}.`);
    }
  }

  function remove(kind: "block" | "section", id: string) {
    commit(kind === "block" ? removeBlock(content, id) : removeSection(content, id));
    if (selected === id) setSelected(null);
    setStatus("Removed.");
  }

  const block = selected === null ? undefined : findById(content, selected);
  const section = selected === null ? undefined : content.sections.find((entry) => entry.id === selected);

  function go(issue: TemplateIssue) {
    const id = issueBlockId(issue.at);
    if (id !== null) {
      setSelected(id);
      setTab("block");
    } else if (issue.at.startsWith("/blocks/settings")) {
      setTab("style");
    } else if (/^\/blocks\/(subject|preheader|sender|lang)/.test(issue.at)) {
      setTab("message");
    }
  }

  return (
    <section className="tp-pane be-pane" aria-label="Editor">
      <div className="be-head">
        <MenuButton
          label="Template mode"
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
                draggable
                onDragStart={(event) => {
                  setDrag({ kind: "new-section", columns });
                  (event.dataTransfer as DataTransfer | null)?.setData("text/plain", "zirv-block");
                }}
                onDragEnd={() => setDrag(null)}
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
                draggable
                onDragStart={(event) => {
                  setDrag({ kind: "new-block", type: entry.type });
                  (event.dataTransfer as DataTransfer | null)?.setData("text/plain", "zirv-block");
                }}
                onDragEnd={() => setDrag(null)}
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
            <MessageForm workspaceId={workspaceId} content={content} readOnly={locked} canAdmin={canAdmin} issues={issues} onChange={commit} onTarget={setTarget} />
          ) : null}
        </div>
      </div>
      <IssueList issues={issues} checked={checked} onGo={(_where, issue) => go(issue)} />
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
