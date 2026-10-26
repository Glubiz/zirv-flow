import { useEffect, useRef, useState, type DragEvent, type ReactNode } from "react";

import type { SnippetOption } from "../../snippets";

import { Button, MenuButton, Sheet } from "../../../ui";
import { AssetPicker, type AssetPick } from "../../assets";
import { IssueList } from "../IssueList";
import { MergeTagPicker } from "../MergeTagPicker";
import { TabBar } from "../TabBar";
import type { Editable, Template, TemplateIssue } from "../model";
import { BlockCanvas } from "./BlockCanvas";
import { BlockSettings, SectionSettings } from "./BlockSettings";
import { MessageForm } from "./DocumentPanel";
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
  newSnippetBlock,
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
  type Block,
  type Drop,
} from "./document";
import type { InsertTarget } from "./insertTarget";
import { SnippetPicker } from "./SnippetPicker";
import { ThemePanel } from "./ThemePanel";
import { NO_THEME, useSnippetNames, useThemeScope, type ThemeScope } from "./themeScope";
import type { SnippetNames } from "./BlockCanvas";

type Blocks = Extract<Editable, { kind: "blocks" }>;
type PanelTab = "block" | "theme" | "message";

/** The dock tabs: the theme's, or for a document with no theme its own style. */
function panelTabs(scope: ThemeScope): readonly { id: PanelTab; label: string }[] {
  return [
    { id: "block", label: "Block settings" },
    { id: "theme", label: scope.ref === "none" ? "Document style" : `Theme: ${scope.name}` },
    { id: "message", label: "Message" },
  ];
}

interface CommonProps {
  workspaceId: string;
  doc: Blocks;
  onChange: (next: Editable) => void;
  readOnly: boolean;
  issues: readonly TemplateIssue[];
  checked: boolean;
}

interface DocumentProps extends CommonProps {
  mode?: "document";
  /** The template, for its UTM override in the Message tab. */
  template: Template;
  onTemplate: (next: Template) => void;
  /** Opens the one-way conversion to MJML code. Absent when the caller cannot convert. */
  onConvert?: () => void;
  /** Select a block or section and show its settings; `n` changes with every request, so asking again for the same one works. */
  goTo?: { id: string; n: number };
}

/** A snippet: one column of blocks, with no sections, no document style and no message settings. */
interface SnippetProps extends CommonProps {
  mode: "snippet";
  /** Shown at the start of the toolbar, before the merge-tag button. */
  toolbar?: ReactNode;
}

type Props = DocumentProps | SnippetProps;

/** The image block's source for what the library gave back. */
function pickedSource(pick: AssetPick) {
  if ("asset" in pick) return { asset: pick.asset.id };
  return "themeLogo" in pick ? { theme: "logo" as const } : { url: pick.url };
}

const NO_SNIPPETS: SnippetNames = new Map();

/** The block editor pane: palette, canvas, a docked settings panel, the validation list and the merge-tag picker. */
export function BlockEditor(props: Props) {
  return props.mode === "snippet" ? <Editor {...props} scope={NO_THEME} snippetNames={NO_SNIPPETS} /> : <DocumentEditor {...props} />;
}

/** A document reads the workspace's themes and snippets to show what it holds. */
function DocumentEditor(props: DocumentProps) {
  const scope = useThemeScope(props.workspaceId, props.doc.content.theme ?? "default");
  const snippetNames = useSnippetNames(props.workspaceId);
  return <Editor {...props} scope={scope} snippetNames={snippetNames} />;
}

function Editor(props: Props & { scope: ThemeScope; snippetNames: SnippetNames }) {
  const { workspaceId, doc, onChange, readOnly, issues, checked, scope, snippetNames } = props;
  const snippet = props.mode === "snippet";
  const tabs = panelTabs(scope);
  const content = doc.content;
  const locked = readOnly || doc.newer;
  const [selected, setSelected] = useState<string | null>(null);
  const [tab, setTab] = useState<PanelTab>("block");
  const [drag, setDrag] = useState<Drag | null>(null);
  const [target, setTarget] = useState<InsertTarget | null>(null);
  const [status, setStatus] = useState("");
  const [picking, setPicking] = useState(false);
  const [inserting, setInserting] = useState(false);
  const [choosingSnippet, setChoosingSnippet] = useState(false);
  const goTo = props.mode === "snippet" ? undefined : props.goTo;
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

  // A request from outside, such as a hidden block in the preview, selects it and brings it into view.
  const requested = useRef(0);
  useEffect(() => {
    if (!goTo || goTo.n === requested.current) return;
    requested.current = goTo.n;
    setSelected(goTo.id);
    setTab("block");
    const own = pane.current?.querySelector<HTMLElement>(`[data-block-id="${goTo.id}"], [data-section-id="${goTo.id}"]`);
    own?.scrollIntoView({ block: "center" });
  }, [goTo]);

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

  function place(block: Block) {
    const { base, place: at } = placeNewBlock(content, selected);
    commit(insertBlock(base, block, at));
    setSelected(block.id);
    setTab("block");
    setStatus(`Added a ${blockLabel(block.type).toLowerCase()} block.`);
  }

  function addBlock(type: PaletteType) {
    if (blockCount(content) >= MAX_BLOCKS) return;
    place(newBlock(type, content));
  }

  function addSnippet(chosen: SnippetOption) {
    setChoosingSnippet(false);
    if (blockCount(content) >= MAX_BLOCKS) return;
    place(newSnippetBlock(chosen.id, content));
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

  function pickImage(pick: AssetPick) {
    setPicking(false);
    if (block?.type !== "image") return;
    commit(updateBlock(content, block.id, () => ({ ...block, src: pickedSource(pick) })));
  }

  function snippetInfoOf(selectedBlock: Block | undefined) {
    if (selectedBlock?.type !== "snippet" || snippet) return undefined;
    const held = snippetNames.get(selectedBlock.snippet_id);
    return held ? { ...held, href: `/w/${workspaceId}/snippets/${selectedBlock.snippet_id}` } : undefined;
  }

  function go(where: string, issue: TemplateIssue) {
    const id = issueBlockId(issue.at);
    if (id !== null) {
      setSelected(id);
      setTab("block");
    } else if (where === "Document style") {
      setTab("theme");
    } else if (where === "Message") {
      setTab("message");
    }
  }

  return (
    <section ref={pane} className="tp-pane be-pane" aria-label="Editor">
      {props.mode === "snippet" ? (
        <div className="be-head">
          {props.toolbar}
          {locked ? null : (
            <Button aria-expanded={inserting} onClick={() => setInserting((open) => !open)}>
              Insert field
            </Button>
          )}
        </div>
      ) : (
        <div className="be-head">
          <MenuButton
            label="Template mode"
            placement="below-start"
            buttonClassName="btn"
            buttonContent="Mode: Blocks"
            entries={props.onConvert ? [{ key: "convert", label: "Convert to MJML…", onSelect: props.onConvert }] : []}
            emptyText="Converting needs permission to edit."
          />
          <span className="faint">One-way: code cannot become blocks again.</span>
        </div>
      )}
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
          {snippet ? null : (
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
          )}
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
            {snippet ? null : (
              <Button aria-label="Add snippet block" onClick={() => setChoosingSnippet(true)} disabled={blockCount(content) >= MAX_BLOCKS}>
                Snippet
              </Button>
            )}
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
        flat={snippet}
        workspaceId={snippet ? undefined : workspaceId}
        snippets={snippetNames}
        footer={
          snippet || !scope.inEffect
            ? undefined
            : {
                theme: scope.name,
                removed: content.footer === "none",
                marketing: content.category === "marketing",
                onRemove: () => commit({ ...content, footer: "none" }),
                onRestore: () => commit({ ...content, footer: "theme" }),
              }
        }
      />
      <div className="be-panel">
        {snippet ? null : <TabBar label="Settings panel" tabs={tabs} tab={tab} onTab={setTab} />}
        <div role="tabpanel" aria-label={tabs.find((entry) => entry.id === tab)?.label} className="stack">
          {tab === "block" && block ? (
            <BlockSettings
              block={block}
              readOnly={locked}
              issues={issues}
              onTarget={setTarget}
              onPickImage={() => setPicking(true)}
              workspaceId={snippet ? undefined : workspaceId}
              snippetInfo={snippetInfoOf(block)}
              onChange={(next) => commit(updateBlock(content, block.id, () => next))}
            />
          ) : null}
          {tab === "block" && section && !snippet ? (
            <SectionSettings workspaceId={workspaceId} section={section} readOnly={locked} issues={issues} onChange={(next) => commit(updateSection(content, section.id, () => next))} />
          ) : null}
          {tab === "block" && !block && (snippet || !section) ? (
            <p className="faint">{snippet ? "Select a block to edit its settings." : "Select a block or a section to edit its settings."}</p>
          ) : null}
          {props.mode !== "snippet" && tab === "theme" ? (
            <ThemePanel workspaceId={workspaceId} scope={scope} content={content} readOnly={locked} issues={issues} onChange={commit} />
          ) : null}
          {props.mode !== "snippet" && tab === "message" ? (
            <MessageForm workspaceId={workspaceId} content={content} readOnly={locked} issues={issues} onChange={commit} onTarget={setTarget} template={props.template} onTemplate={props.onTemplate} />
          ) : null}
        </div>
      </div>
      <IssueList issues={issues} checked={checked} onGo={go} />
      {locked ? null : snippet ? (
        <>
          {inserting ? (
            <Sheet title="Insert a field" autoFocus={false} onClose={() => setInserting(false)}>
              <MergeTagPicker
                workspaceId={workspaceId}
                disabled={target === null}
                target={target === null ? "a text field" : target.label}
                onInsert={(tag) => target?.insert(tag)}
              />
            </Sheet>
          ) : null}
          <AssetPicker open={picking} themeLogo onPick={pickImage} onClose={() => setPicking(false)} />
        </>
      ) : (
        <>
          <MergeTagPicker
            workspaceId={workspaceId}
            theme={scope.inEffect}
            disabled={target === null}
            target={target === null ? "a text field" : target.label}
            onInsert={(tag) => target?.insert(tag)}
          />
          <AssetPicker open={picking} themeLogo={scope.inEffect} onPick={pickImage} onClose={() => setPicking(false)} />
          {choosingSnippet ? <SnippetPicker workspaceId={workspaceId} onPick={addSnippet} onClose={() => setChoosingSnippet(false)} /> : null}
        </>
      )}
    </section>
  );
}
