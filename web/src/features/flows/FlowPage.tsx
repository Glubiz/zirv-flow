import { useMemo, useRef, useState, type KeyboardEvent } from "react";
import { Link, useParams } from "react-router";

import { ApiError, client, describeError, unwrap, useResource } from "../../api";
import { Button, Dialog, EmptyState, ErrorState, LoadingState, Panel } from "../../ui";
import { useCatalog, useLeaveGuard } from "../segments";
import { useFlowAccess } from "./access";
import { Canvas, type FocusRequest, type IssueItem } from "./Canvas";
import { wordsFor, triggerWords } from "./describe";
import { StatePill } from "./FlowsPage";
import { placeIssues, SECTIONS, type Section } from "./issues";
import {
  addNode,
  edgeFrom,
  kindLabel,
  removeIndexedPort,
  removeNode,
  replaceNode,
  TRIGGER_ID,
  unwire,
  wire,
  type FlowDetail,
  type FlowNode,
  type Graph,
  type NodeType,
  type Settings,
} from "./model";
import { layout } from "./layout";
import { SettingsView, type SettingsFocus } from "./SettingsView";
import { StepSheet } from "./StepSheet";
import { useFlowDraft } from "./useFlowDraft";

const TABS = [
  { id: "canvas", label: "Canvas" },
  { id: "settings", label: "Flow settings" },
  { id: "versions", label: "Versions" },
] as const;

type TabId = (typeof TABS)[number]["id"];

export function FlowPage() {
  const { ws = "", id = "" } = useParams();
  return <FlowLoader key={`${ws}/${id}`} workspaceId={ws} flowId={id} />;
}

function FlowLoader({ workspaceId, flowId }: { workspaceId: string; flowId: string }) {
  const access = useFlowAccess(workspaceId);
  const flow = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/flows/{id}", { params: { path: { ws: workspaceId, id: flowId } } })),
    [workspaceId, flowId],
  );
  const back = (
    <p>
      <Link to={`/w/${workspaceId}/flows`}>All flows</Link>
    </p>
  );

  if (flow.status === "loading" || (flow.status === "ready" && !access.ready)) {
    return (
      <div className="page">
        {back}
        <LoadingState label="Loading the flow" />
      </div>
    );
  }
  if (flow.status === "error") {
    const missing = flow.error instanceof ApiError && flow.error.status === 404;
    return (
      <div className="page">
        {back}
        <Panel flush>
          {missing ? (
            <EmptyState title="This flow does not exist">It may have been deleted, or the address is wrong.</EmptyState>
          ) : (
            <ErrorState {...describeError(flow.error)} onRetry={flow.reload} />
          )}
        </Panel>
      </div>
    );
  }
  return <Editor workspaceId={workspaceId} flow={flow.data} canWrite={access.canWrite} />;
}

function stateLabel(state: FlowDetail["state"], version: number | null): string | null {
  if (state === "published") return version === null ? null : `v${version} live`;
  if (state === "paused") return version === null ? null : `v${version}`;
  return null;
}

function Editor({ workspaceId, flow, canWrite }: { workspaceId: string; flow: FlowDetail; canWrite: boolean }) {
  const archived = flow.state === "archived";
  const editable = canWrite && !archived;
  const store = useFlowDraft({ workspaceId, flow, editable });
  const { draft } = store;
  const catalogState = useCatalog(workspaceId);
  const catalog = catalogState.status === "ready" ? catalogState.catalog : null;
  const names = useMemo(() => ({ lists: catalog?.lists ?? [], segments: catalog?.segments ?? [] }), [catalog]);
  const guard = useLeaveGuard(store.unsaved);

  const [tab, setTab] = useState<TabId>("canvas");
  const [selected, setSelected] = useState<string | null>(null);
  const [sheetFocus, setSheetFocus] = useState(true);
  const [focusRequest, setFocusRequest] = useState<FocusRequest | null>(null);
  const [settingsFocus, setSettingsFocus] = useState<SettingsFocus | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);
  const nonce = useRef(0);

  const placed = useMemo(() => placeIssues(store.issues ?? []), [store.issues]);
  const plan = useMemo(() => layout(draft.graph), [draft.graph]);
  const steps = useMemo(
    () => plan.order.filter((id) => id !== TRIGGER_ID).map((id, index) => ({ id, step: index + 1 })),
    [plan],
  );

  function describeStep(id: string): string {
    if (id === TRIGGER_ID) return "Trigger";
    const node = draft.graph.nodes.find((candidate) => candidate.id === id);
    return node ? `${kindLabel(node.type)}: ${wordsFor(node, names).title}` : "A step";
  }

  const issueItems: IssueItem[] = (store.issues ?? []).map((issue, index) => {
    let where = "The whole flow";
    if (issue.node !== undefined) {
      where = issue.node === TRIGGER_ID ? "Trigger" : describeStep(issue.node);
    } else if (issue.edge !== undefined) {
      const edge = draft.graph.edges.find((candidate) => candidate.id === issue.edge);
      where = edge ? `Connection from ${describeStep(edge.from)}` : "A connection";
    } else {
      const section = placed.bySection;
      for (const entry of SECTIONS) {
        if (section.get(entry.id)?.includes(issue)) where = entry.label;
      }
    }
    return { key: `${index}:${issue.code}:${issue.node ?? issue.edge ?? issue.at ?? ""}`, where, issue };
  });

  function focusStep(id: string, takeFocus: boolean) {
    nonce.current += 1;
    setTab("canvas");
    setSelected(id);
    setSheetFocus(takeFocus);
    if (!takeFocus) setFocusRequest({ id, nonce: nonce.current });
  }

  function openSettings(section: Section) {
    nonce.current += 1;
    setSelected(null);
    setTab("settings");
    setSettingsFocus({ section, nonce: nonce.current });
  }

  function open(id: string) {
    if (id === TRIGGER_ID) openSettings("trigger");
    else focusStep(id, true);
  }

  function closeSheet() {
    if (selected !== null) {
      nonce.current += 1;
      setFocusRequest({ id: selected, nonce: nonce.current });
    }
    setSelected(null);
  }

  function pick(item: IssueItem) {
    const { issue } = item;
    if (issue.node !== undefined) {
      focusStep(issue.node, false);
    } else if (issue.edge !== undefined) {
      const edge = draft.graph.edges.find((candidate) => candidate.id === issue.edge);
      if (edge) focusStep(edge.from, false);
    } else {
      const entry = SECTIONS.find((section) => placed.bySection.get(section.id)?.includes(issue));
      if (entry) openSettings(entry.id);
    }
  }

  const editGraph = (change: (graph: Graph) => Graph, key?: string) =>
    store.edit((current) => ({ ...current, graph: change(current.graph) }), key);
  const editSettings = (change: (settings: Settings) => Settings, key?: string) =>
    store.edit((current) => ({ ...current, settings: change(current.settings) }), key);

  function add(type: NodeType) {
    const { id } = addNode(draft.graph, type);
    editGraph((graph) => addNode(graph, type).graph);
    focusStep(id, true);
  }

  function wireStep(from: string, port: string, to: string | null) {
    if (to === null) {
      const edge = edgeFrom(draft.graph, from, port);
      if (edge) editGraph((graph) => unwire(graph, edge.id));
      return;
    }
    editGraph((graph) => wire(graph, from, port, to));
  }

  function confirmDelete() {
    if (deleting === null) return;
    const id = deleting;
    editGraph((graph) => removeNode(graph, id));
    setDeleting(null);
    setSelected(null);
    nonce.current += 1;
    setFocusRequest({ id: TRIGGER_ID, nonce: nonce.current });
  }

  const sheetNode = selected === null ? undefined : draft.graph.nodes.find((candidate) => candidate.id === selected);
  const doomed = deleting === null ? undefined : draft.graph.nodes.find((candidate) => candidate.id === deleting);

  function onTabKey(event: KeyboardEvent<HTMLDivElement>) {
    const at = TABS.findIndex((entry) => entry.id === tab);
    const last = TABS.length - 1;
    let to: number;
    if (event.key === "ArrowRight") to = at === last ? 0 : at + 1;
    else if (event.key === "ArrowLeft") to = at === 0 ? last : at - 1;
    else if (event.key === "Home") to = 0;
    else if (event.key === "End") to = last;
    else return;
    event.preventDefault();
    const next = TABS[to];
    if (next) {
      setTab(next.id);
      document.getElementById(`flow-tab-${next.id}`)?.focus();
    }
  }

  const version = stateLabel(store.meta.state, store.meta.currentVersion);

  return (
    <div className="fl-page">
      <div className="fl-head">
        <Link to={`/w/${workspaceId}/flows`}>← Flows</Link>
        {editable ? (
          <NameField name={draft.name} onCommit={(name) => store.edit((current) => ({ ...current, name }), "name")} />
        ) : (
          <h1 className="fl-name-read">{draft.name}</h1>
        )}
        <span className="fl-state">
          <StatePill state={store.meta.state} />
          {version ? <span className="faint">{version}</span> : null}
        </span>
        <div className="fl-tabs" role="tablist" aria-label="Flow views" onKeyDown={onTabKey}>
          {TABS.map((entry) => (
            <button
              key={entry.id}
              type="button"
              role="tab"
              id={`flow-tab-${entry.id}`}
              className="fl-tab"
              aria-selected={tab === entry.id}
              aria-controls={`flow-panel-${entry.id}`}
              tabIndex={tab === entry.id ? 0 : -1}
              onClick={() => setTab(entry.id)}
            >
              {entry.label}
            </button>
          ))}
        </div>
        <div className="fl-status" role="status">
          {!editable ? (
            <span className="muted">{archived ? "This flow is archived. You can look at it but not change it." : "You can look at this flow but not change it."}</span>
          ) : store.status === "saving" ? (
            <span className="muted">Saving…</span>
          ) : store.status === "error" ? (
            <span className="fl-status-error">Couldn&rsquo;t save</span>
          ) : store.saves > 0 ? (
            <span className="muted">Saved</span>
          ) : null}
        </div>
        {store.status === "error" ? (
          <div className="fl-retry">
            {store.failure ? <span className="faint">{store.failure.detail}</span> : null}
            <Button onClick={store.retry}>Retry</Button>
          </div>
        ) : null}
      </div>

      {tab === "canvas" ? (
        <div role="tabpanel" id="flow-panel-canvas" aria-labelledby="flow-tab-canvas">
          <Canvas
            graph={draft.graph}
            settings={draft.settings}
            names={names}
            placed={placed}
            issueItems={issueItems}
            checked={store.issues !== null}
            editable={editable}
            selectedId={sheetNode ? sheetNode.id : null}
            focusRequest={focusRequest}
            canUndo={store.canUndo}
            onOpen={open}
            onClose={closeSheet}
            onWire={(from, port, to) => wireStep(from, port, to)}
            onAdd={add}
            onUndo={store.undo}
            onDeleteRequest={(id) => setDeleting(id)}
            onPickIssue={pick}
          >
            {sheetNode ? (
              <div className="fl-sheet">
                <StepSheet
                  workspaceId={workspaceId}
                  graph={draft.graph}
                  node={sheetNode}
                  names={names}
                  steps={steps}
                  issues={placed.byNode.get(sheetNode.id) ?? []}
                  catalog={catalog}
                  readOnly={!editable}
                  autoFocus={sheetFocus}
                  onClose={closeSheet}
                  onChange={(next: FlowNode, key) => editGraph((graph) => replaceNode(graph, next), key)}
                  onRemovePort={(next, prefix, index) => editGraph((graph) => removeIndexedPort(graph, next, prefix, index))}
                  onWire={(port, to) => wireStep(sheetNode.id, port, to)}
                  onDelete={() => setDeleting(sheetNode.id)}
                />
              </div>
            ) : null}
          </Canvas>
        </div>
      ) : null}

      {tab === "settings" ? (
        <div role="tabpanel" id="flow-panel-settings" aria-labelledby="flow-tab-settings" className="fl-pane">
          <p className="faint">
            Starts with: {triggerWords(draft.settings, names).title}. Changes here are saved as you make them.
          </p>
          <SettingsView
            workspaceId={workspaceId}
            settings={draft.settings}
            catalog={catalog}
            readOnly={!editable}
            issues={placed.bySection}
            focus={settingsFocus}
            onChange={editSettings}
          />
        </div>
      ) : null}

      {tab === "versions" ? (
        <div role="tabpanel" id="flow-panel-versions" aria-labelledby="flow-tab-versions" className="fl-pane">
          <Panel flush>
            <EmptyState title="Version history comes next">Each published version will be listed here, with its steps to look at.</EmptyState>
          </Panel>
        </div>
      ) : null}

      {doomed ? (
        <Dialog title="Delete this step?" onClose={() => setDeleting(null)}>
          <p className="muted">
            {kindLabel(doomed.type)}: {wordsFor(doomed, names).title} and its connections will be removed. You can undo this until you leave the page.
          </p>
          <div className="dialog-actions">
            <Button onClick={() => setDeleting(null)}>Cancel</Button>
            <Button variant="primary" onClick={confirmDelete}>
              Delete step
            </Button>
          </div>
        </Dialog>
      ) : null}

      {guard.pending !== null ? (
        <Dialog title="Leave without saving?" onClose={guard.stay}>
          <p className="muted">Your latest changes have not been saved yet. If you leave now, they are lost.</p>
          <div className="dialog-actions">
            <Button onClick={guard.stay}>Keep editing</Button>
            <Button variant="primary" onClick={guard.leave}>
              Leave without saving
            </Button>
          </div>
        </Dialog>
      ) : null}
    </div>
  );
}

/** The name of the flow as a heading that can be typed into; the change is made on Enter or on leaving. */
function NameField({ name, onCommit }: { name: string; onCommit: (name: string) => void }) {
  const [text, setText] = useState(name);
  const [seen, setSeen] = useState(name);
  if (seen !== name) {
    setSeen(name);
    setText(name);
  }
  function commit() {
    const trimmed = text.trim();
    if (trimmed === "") {
      setText(name);
    } else if (trimmed !== name) {
      onCommit(trimmed);
    }
  }
  return (
    <h1 className="fl-name">
      <input
        className="fl-name-input"
        aria-label="Flow name"
        value={text}
        maxLength={100}
        autoComplete="off"
        onChange={(event) => setText(event.target.value)}
        onBlur={commit}
        onKeyDown={(event) => {
          if (event.key === "Enter") {
            event.preventDefault();
            commit();
          } else if (event.key === "Escape") {
            setText(name);
          }
        }}
      />
    </h1>
  );
}
