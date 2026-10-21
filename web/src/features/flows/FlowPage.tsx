import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import { Link, useParams } from "react-router";

import { ApiError, client, describeError, unwrap, useResource, type components, type ErrorSummary } from "../../api";
import { Button, Dialog, EmptyState, ErrorState, LoadingState, Panel } from "../../ui";
import { useAssistantAccess } from "../assistant";
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
  portLabel,
  removeIndexedPort,
  removeNode,
  replaceNode,
  TRIGGER_ID,
  unwire,
  wire,
  type FlowDetail,
  type FlowIssue,
  type FlowNode,
  type Graph,
  type NodeType,
  type Settings,
} from "./model";
import { layout } from "./layout";
import { PublishDialog } from "./PublishDialog";
import { VersionCanvas, VersionsView, type FlowVersion } from "./VersionsView";
import { SettingsView, type SettingsFocus } from "./SettingsView";
import { FlowAssistantSheet } from "./FlowAssistant";
import { Inspector } from "./Inspector";
import { pathOverlay, type Overlay } from "./overlay";
import { StepSheet } from "./StepSheet";
import { NOTHING_SENT, TestSheet } from "./TestSheet";
import type { TestResult } from "./testMode";
import { useFlowDraft } from "./useFlowDraft";

/** The settings section an issue about no step or connection belongs to, if any. */
function sectionOfIssue(issue: FlowIssue) {
  const found = placeIssues([issue]).bySection;
  return SECTIONS.find((section) => found.get(section.id)?.length);
}

type FlowBody = components["schemas"]["FlowBody"];

const TABS = [
  { id: "canvas", label: "Canvas" },
  { id: "settings", label: "Flow settings" },
  { id: "versions", label: "Versions" },
  { id: "enrollments", label: "Enrollments" },
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
  return <Editor workspaceId={workspaceId} flow={flow.data} canWrite={access.canWrite} canAdmin={access.canAdmin} />;
}

function stateLabel(state: FlowDetail["state"], version: number | null): string | null {
  if (state === "published") return version === null ? null : `v${version} live`;
  if (state === "paused") return version === null ? null : `v${version}`;
  return null;
}

interface Life {
  state: FlowDetail["state"];
  version: number | null;
}

function Editor({
  workspaceId,
  flow,
  canWrite,
  canAdmin,
}: {
  workspaceId: string;
  flow: FlowDetail;
  canWrite: boolean;
  canAdmin: boolean;
}) {
  const [life, setLife] = useState<Life>({ state: flow.state, version: flow.current_version ?? null });
  const archived = life.state === "archived";
  const editable = canWrite && !archived;
  const store = useFlowDraft({ workspaceId, flow, editable });
  const { draft } = store;
  const catalogState = useCatalog(workspaceId);
  const catalog = catalogState.status === "ready" ? catalogState.catalog : null;
  const names = useMemo(() => ({ lists: catalog?.lists ?? [], segments: catalog?.segments ?? [] }), [catalog]);
  const guard = useLeaveGuard(store.unsaved);
  /** The assistant is for admins, and only once the workspace has it on. */
  const { enabled: assistantOn, reload: reloadAssistant } = useAssistantAccess(workspaceId, canAdmin);
  /** An accepted draft is being loaded: the editor is read-only until it lands or is given up. */
  const busy = store.held;
  const canEdit = editable && !busy;

  const [tab, setTab] = useState<TabId>("canvas");
  const [selected, setSelected] = useState<string | null>(null);
  const [sheetFocus, setSheetFocus] = useState(true);
  const [focusRequest, setFocusRequest] = useState<FocusRequest | null>(null);
  const [settingsFocus, setSettingsFocus] = useState<SettingsFocus | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);
  const [publishOpen, setPublishOpen] = useState(false);
  const [inFlight, setInFlight] = useState(false);
  const [refusal, setRefusal] = useState<ErrorSummary | null>(null);
  const [refusedIssues, setRefusedIssues] = useState<FlowIssue[] | null>(null);
  /** Read after an await, where the state of the render that started the publish is stale. */
  const publishOpenRef = useRef(false);
  const [announcement, setAnnouncement] = useState("");
  const [viewing, setViewing] = useState<FlowVersion | null>(null);
  const [archiving, setArchiving] = useState(false);
  const [acting, setActing] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<TestResult | null>(null);
  const [assistantOpen, setAssistantOpen] = useState(false);
  /** Bumped when a whole new draft is loaded, so the canvas draws it afresh. */
  const [layoutEpoch, setLayoutEpoch] = useState(0);
  /** The steps the assistant's explanation cites, marked on the canvas while it is open. */
  const [cited, setCited] = useState<readonly string[]>([]);
  const [assistantSeen, setAssistantSeen] = useState(assistantOn);
  if (assistantSeen !== assistantOn) {
    setAssistantSeen(assistantOn);
    // Turned off, or no longer the person's to use: the sheet and what it marked on the canvas go with it.
    if (!assistantOn) {
      setAssistantOpen(false);
      setCited([]);
    }
  }
  /** Set while the person is asked whether the assistant's draft may replace their unsaved edits. */
  const [replacing, setReplacing] = useState<{ resolve: (replace: boolean) => void } | null>(null);
  const nonce = useRef(0);
  const announceTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  /** The editor's own element, so focus is only ever moved inside it. */
  const pageRef = useRef<HTMLDivElement>(null);
  /** False once the editor is gone: a late response then moves no focus and starts no timer. */
  const mounted = useRef(false);
  /** Requests that change the flow's state are numbered as they start; a response older than one already applied is dropped. */
  const requests = useRef(0);
  const appliedRequest = useRef(0);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      clearTimeout(announceTimer.current);
    };
  }, []);
  useEffect(() => {
    publishOpenRef.current = publishOpen;
  }, [publishOpen]);

  const placed = useMemo(() => placeIssues(store.issues ?? []), [store.issues]);
  const plan = useMemo(() => layout(draft.graph), [draft.graph]);
  const steps = useMemo(
    () => plan.order.filter((id) => id !== TRIGGER_ID).map((id, index) => ({ id, step: index + 1 })),
    [plan],
  );

  const testOverlay = useMemo<Overlay | undefined>(
    () => (testResult ? pathOverlay(draft.graph, testResult.path) : undefined),
    [testResult, draft.graph],
  );

  const citedOverlay = useMemo<Overlay | undefined>(
    () =>
      assistantOpen && cited.length > 0
        ? { steps: new Map(cited.map((id) => [id, [] as number[]])), dimOthers: true }
        : undefined,
    [assistantOpen, cited],
  );

  function describeStep(id: string): string {
    if (id === TRIGGER_ID) return "Trigger";
    const node = draft.graph.nodes.find((candidate) => candidate.id === id);
    return node ? `${kindLabel(node.type)}: ${wordsFor(node, names).title}` : "A step";
  }

  /** The issues as entries, each with where it is in words and whether a step or section can be opened for it. */
  function itemsFor(issues: readonly FlowIssue[]): IssueItem[] {
    return issues.map((issue, index) => {
      let where = "The whole flow";
      let openable = false;
      if (issue.node !== undefined) {
        where = issue.node === TRIGGER_ID ? "Trigger" : describeStep(issue.node);
        openable = true;
      } else if (issue.edge !== undefined) {
        const edge = draft.graph.edges.find((candidate) => candidate.id === issue.edge);
        where = edge ? `Connection from ${describeStep(edge.from)}` : "A connection";
        openable = edge !== undefined && draft.graph.nodes.some((node) => node.id === edge.from);
      } else {
        const entry = sectionOfIssue(issue);
        if (entry) {
          where = entry.label;
          openable = true;
        }
      }
      return { key: `${index}:${issue.code}:${issue.node ?? issue.edge ?? issue.at ?? ""}`, where, issue, openable };
    });
  }

  const issueItems = itemsFor(store.issues ?? []);

  /** The issues of a step, with those of its outgoing connections placed on the outputs they leave. */
  function issuesOfStep(id: string): FlowIssue[] {
    const own = placed.byNode.get(id) ?? [];
    const onWires = draft.graph.edges
      .filter((edge) => edge.from === id)
      .flatMap((edge) => (placed.byEdge.get(edge.id) ?? []).map((issue) => ({ ...issue, at: `/ports/${edge.port}` })));
    return [...own, ...onWires];
  }

  function focusStep(id: string, takeFocus: boolean) {
    nonce.current += 1;
    setTab("canvas");
    setSelected(id);
    setSheetFocus(takeFocus);
    if (!takeFocus) setFocusRequest({ id, nonce: nonce.current });
  }

  /** Moves focus to a step without opening its sheet, so the assistant's sheet stays. */
  function revealStep(id: string) {
    nonce.current += 1;
    setFocusRequest({ id, nonce: nonce.current, reveal: true });
  }

  function toggleAssistant() {
    if (assistantOpen && selected === null) {
      setAssistantOpen(false);
      setCited([]);
      return;
    }
    setSelected(null);
    setAssistantOpen(true);
  }

  /** Accepting replaces the draft, so edits that are not saved yet are asked about first, then the editor stands still. */
  async function confirmAssistantAccept(): Promise<boolean> {
    if (store.unsaved) {
      const replace = await new Promise<boolean>((resolve) => setReplacing({ resolve }));
      if (!replace) return false;
    }
    await store.hold();
    return true;
  }

  /** An accept failed without a known outcome: the server says whether the draft landed. */
  async function reconcileAccept(): Promise<boolean> {
    const outcome = await store.reconcile();
    if (outcome !== "adopted") return false;
    setSelected(null);
    setCited([]);
    setLayoutEpoch((epoch) => epoch + 1);
    announce("The draft was accepted, but the reply was lost. It is loaded and saved as the flow's draft. Nothing is published.");
    return true;
  }

  function answerReplace(replace: boolean) {
    replacing?.resolve(replace);
    setReplacing(null);
  }

  /** The server has written the accepted draft as the flow's draft; the editor shows it. Nothing is published. */
  function loadAssistantDraft(content: unknown) {
    const document = content as { graph?: Graph; settings?: Settings } | null;
    if (!document?.graph || !document.settings) {
      store.release();
      return;
    }
    store.adopt({ graph: document.graph, settings: document.settings });
    setSelected(null);
    setCited([]);
    setLayoutEpoch((epoch) => epoch + 1);
    announce("The assistant's draft is loaded and saved as the flow's draft. Nothing is published.");
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
      const entry = sectionOfIssue(issue);
      if (entry) openSettings(entry.id);
    }
  }

  /** Closes the publish dialog and shows what one of its issues is about. */
  function pickFromDialog(item: IssueItem) {
    setPublishOpen(false);
    pick(item);
  }

  /** After an action the button that had focus may be gone, so focus goes to the first action or the tab in use. */
  function refocusActions() {
    if (!mounted.current) return;
    setTimeout(() => {
      const page = pageRef.current;
      if (!mounted.current || !page) return;
      (page.querySelector<HTMLElement>("#flow-primary-action") ?? page.querySelector<HTMLElement>(`#flow-tab-${tab}`))?.focus();
    }, 0);
  }

  /** Numbers a request that is about to start. */
  function startRequest(): number {
    requests.current += 1;
    return requests.current;
  }

  /** Shows the flow state a response carries, unless a newer request's state is already shown. */
  function applyLife(request: number, next: Life): boolean {
    if (request < appliedRequest.current) return false;
    appliedRequest.current = request;
    setLife(next);
    return true;
  }

  function noticeOf(error: unknown) {
    const summary = describeError(error);
    setNotice(summary.detail || summary.title);
  }

  /** Says something to a screen reader; the region is cleared first so the same words are read again. */
  function announce(text: string) {
    if (!mounted.current) return;
    clearTimeout(announceTimer.current);
    setAnnouncement("");
    announceTimer.current = setTimeout(() => setAnnouncement(text), 0);
  }

  /** The flow changed somewhere else (a 409): shows it as it is now, with a notice. */
  async function adoptCurrent(withNotice = true) {
    const request = startRequest();
    try {
      const current = await unwrap(
        client.GET("/api/v1/workspaces/{ws}/flows/{id}", { params: { path: { ws: workspaceId, id: flow.id } } }),
      );
      if (applyLife(request, { state: current.state, version: current.current_version ?? null }) && withNotice) {
        setNotice("The flow was changed somewhere else. It is shown as it is now; nothing was done.");
      }
    } catch (again) {
      noticeOf(again);
    }
  }

  async function lifecycle(action: "pause" | "resume" | "archive") {
    if (acting || inFlight || busy) return;
    const request = startRequest();
    setActing(true);
    setNotice(null);
    const path = { params: { path: { ws: workspaceId, id: flow.id } } };
    try {
      // Pending edits go in first: an archived flow takes no more, so a failed save stops the archive.
      if (action === "archive" && !(await store.saveNow())) {
        setNotice("Your latest changes could not be saved, so the flow was not archived. Try again once they save.");
        return;
      }
      const updated = await unwrap(
        action === "pause"
          ? client.POST("/api/v1/workspaces/{ws}/flows/{id}/pause", path)
          : action === "resume"
            ? client.POST("/api/v1/workspaces/{ws}/flows/{id}/resume", path)
            : client.POST("/api/v1/workspaces/{ws}/flows/{id}/archive", path),
      );
      applyLife(request, { state: updated.state, version: updated.current_version ?? null });
      announce(action === "pause" ? "The flow is paused." : action === "resume" ? "The flow is live again." : "The flow is archived.");
    } catch (error) {
      if (error instanceof ApiError && error.status === 409) {
        await adoptCurrent();
      } else {
        noticeOf(error);
      }
    } finally {
      setActing(false);
      setArchiving(false);
      refocusActions();
    }
  }

  function openPublish() {
    if (inFlight || acting || busy) return;
    setRefusal(null);
    setRefusedIssues(null);
    setPublishOpen(true);
  }

  /** The editor owns the request: the dialog may be closed while it is on its way, and the outcome still lands. */
  async function startPublish() {
    if (inFlight || acting) return;
    const request = startRequest();
    setInFlight(true);
    setRefusal(null);
    try {
      const result = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/flows/{id}/publish", { params: { path: { ws: workspaceId, id: flow.id } } }),
      );
      published(request, result as { flow: FlowBody; version: { version: number } });
    } catch (error) {
      if (error instanceof ApiError && error.status === 409) {
        publishConflict();
      } else if (error instanceof ApiError && error.code === "flow_invalid") {
        const found = error.flowIssues.map((issue) => issue as FlowIssue);
        store.adoptIssues(found);
        setRefusedIssues(found);
        if (!publishOpenRef.current) setNotice("The flow has issues, so it was not published. They are marked on the canvas.");
      } else {
        setRefusal(describeError(error));
        if (!publishOpenRef.current) noticeOf(error);
      }
    } finally {
      setInFlight(false);
    }
  }

  function published(request: number, result: { flow: FlowBody; version: { version: number } }) {
    if (!mounted.current) return;
    setPublishOpen(false);
    setViewing(null);
    setNotice(null);
    applyLife(request, { state: result.flow.state, version: result.flow.current_version ?? result.version.version });
    refocusActions();
    announce(`Version ${result.version.version} of ${draft.name} is published.`);
  }

  /** A publish was refused with a 409: the dialog goes and the flow is shown as it is now. */
  function publishConflict() {
    if (!mounted.current) return;
    setPublishOpen(false);
    setNotice(null);
    void adoptCurrent().then(refocusActions);
  }

  function view(version: FlowVersion) {
    setViewing(version);
    announce(`Viewing version ${version.version}, read-only.`);
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

  /** Switching tabs leaves a version being looked at. */
  function chooseTab(next: TabId) {
    setViewing(null);
    setCited([]);
    setTesting(false);
    setTestResult(null);
    setTab(next);
  }

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
      chooseTab(next.id);
      document.getElementById(`flow-tab-${next.id}`)?.focus();
    }
  }

  function toggleTest() {
    if (testing) {
      exitTest();
      return;
    }
    setViewing(null);
    setSelected(null);
    setAssistantOpen(false);
    setCited([]);
    setTab("canvas");
    setTesting(true);
    announce("Test mode is on. Nothing is sent or changed.");
  }

  function exitTest() {
    setTesting(false);
    setTestResult(null);
    announce("Test mode is off.");
    setTimeout(() => {
      if (mounted.current) document.getElementById("flow-test-toggle")?.focus();
    }, 0);
  }

  function portName(id: string, port: string): string | null {
    const node = draft.graph.nodes.find((candidate) => candidate.id === id);
    return node ? portLabel(node, port) : null;
  }

  const version = stateLabel(life.state, life.version);

  return (
    <div className="fl-page" ref={pageRef} aria-busy={busy || undefined}>
      <div className="fl-head">
        <Link to={`/w/${workspaceId}/flows`}>← Flows</Link>
        {editable ? (
          <NameField disabled={busy} name={draft.name} onCommit={(name) => store.edit((current) => ({ ...current, name }), "name")} />
        ) : (
          <h1 className="fl-name-read">{draft.name}</h1>
        )}
        <span className="fl-state">
          <StatePill state={life.state} />
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
              aria-controls={tab === entry.id ? "flow-panel" : undefined}
              tabIndex={tab === entry.id ? 0 : -1}
              onClick={() => chooseTab(entry.id)}
            >
              {entry.label}
            </button>
          ))}
        </div>
        <div className="fl-status" role="status">
          {busy ? (
            <span className="muted">Loading the draft…</span>
          ) : !editable ? (
            <span className="muted">{archived ? "This flow is archived. You can look at it but not change it." : "You can look at this flow but not change it."}</span>
          ) : store.status === "saving" ? (
            <span className="muted">Saving…</span>
          ) : store.status === "error" ? (
            <span className="fl-status-error">Couldn&rsquo;t save</span>
          ) : store.saves > 0 ? (
            <span className="muted">Saved</span>
          ) : null}
        </div>
        {canWrite ? (
          <Button id="flow-test-toggle" aria-pressed={testing} onClick={toggleTest}>
            {testing ? "Exit test" : "Test"}
          </Button>
        ) : null}
        {canAdmin && life.state !== "archived" ? (
          <div className="fl-actions">
            <Button id="flow-primary-action" variant="primary" disabled={acting || busy} aria-disabled={inFlight || undefined} onClick={openPublish}>
              {inFlight ? "Publishing…" : "Publish…"}
            </Button>
            {life.state === "published" ? (
              <Button disabled={acting || busy} aria-disabled={inFlight || undefined} onClick={() => void lifecycle("pause")}>
                Pause
              </Button>
            ) : null}
            {life.state === "paused" ? (
              <Button disabled={acting || busy} aria-disabled={inFlight || undefined} onClick={() => void lifecycle("resume")}>
                Resume
              </Button>
            ) : null}
            <Button disabled={acting || busy} aria-disabled={inFlight || undefined} onClick={() => !inFlight && setArchiving(true)}>
              Archive…
            </Button>
          </div>
        ) : null}
        {store.status === "error" ? (
          <div className="fl-retry">
            {store.failure ? <span className="faint">{store.failure.detail}</span> : null}
            <Button disabled={busy} onClick={store.retry}>
              Retry
            </Button>
          </div>
        ) : null}
      </div>

      {notice ? (
        <p className="fl-notice" role="alert">
          {notice}
        </p>
      ) : null}

      {testing ? (
        <div className="fl-banner" role="note">
          <span className="strong">Test mode</span>
          <span>{NOTHING_SENT}</span>
        </div>
      ) : null}

      <div role="tabpanel" id="flow-panel" aria-labelledby={`flow-tab-${tab}`} tabIndex={0} className="fl-panel">
      {tab === "canvas" ? (
          <Canvas
            graph={draft.graph}
            settings={draft.settings}
            names={names}
            placed={placed}
            issueItems={issueItems}
            checked={store.issues !== null}
            checking={store.checking}
            editable={canEdit && !testing}
            layoutKey={layoutEpoch}
            overlay={testOverlay ?? citedOverlay}
            assistant={assistantOn && !testing ? { open: assistantOpen, onToggle: toggleAssistant } : undefined}
            selectedId={sheetNode && !testing ? sheetNode.id : null}
            focusRequest={focusRequest}
            canUndo={store.canUndo}
            onOpen={testing ? () => undefined : open}
            onClose={closeSheet}
            onWire={(from, port, to) => wireStep(from, port, to)}
            onAdd={add}
            onUndo={store.undo}
            onDeleteRequest={(id) => setDeleting(id)}
            onPickIssue={pick}
          >
            {testing ? (
              <div className="fl-sheet">
                <TestSheet
                  workspaceId={workspaceId}
                  flowId={flow.id}
                  canSeePeople={canAdmin}
                  describeStep={describeStep}
                  portName={portName}
                  graph={draft.graph}
                  settingsUnsaved={store.settingsUnsaved}
                  onResult={setTestResult}
                  onClose={exitTest}
                  announce={announce}
                />
              </div>
            ) : null}
            {assistantOn && !testing && assistantOpen ? (
              <div className="fl-sheet" hidden={sheetNode !== undefined}>
                <FlowAssistantSheet
                  workspaceId={workspaceId}
                  flowId={flow.id}
                  version={null}
                  graph={draft.graph}
                  revision={[draft.graph, draft.settings]}
                  names={names}
                  onGone={reloadAssistant}
                  busy={busy}
                  drafting={
                    editable
                      ? { confirmAccept: confirmAssistantAccept, onAccept: loadAssistantDraft, onAcceptFailed: reconcileAccept }
                      : undefined
                  }
                  prepare={editable ? store.saveNow : undefined}
                  onReveal={revealStep}
                  onCited={setCited}
                  onClose={toggleAssistant}
                />
              </div>
            ) : null}
            {!testing && sheetNode ? (
              <div className="fl-sheet">
                <StepSheet
                  key={sheetNode.id}
                  workspaceId={workspaceId}
                  graph={draft.graph}
                  node={sheetNode}
                  names={names}
                  steps={steps}
                  issues={issuesOfStep(sheetNode.id)}
                  catalog={catalog}
                  readOnly={!canEdit}
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
      ) : null}

      {tab === "settings" ? (
        <div className="fl-pane">
          <p className="faint">
            Starts with: {triggerWords(draft.settings, names).title}. Changes here are saved as you make them.
          </p>
          <SettingsView
            workspaceId={workspaceId}
            settings={draft.settings}
            catalog={catalog}
            readOnly={!canEdit}
            issues={placed.bySection}
            focus={settingsFocus}
            onChange={editSettings}
          />
        </div>
      ) : null}

      {tab === "versions" && viewing === null ? (
        <div className="fl-pane">
          <VersionsView
            workspaceId={workspaceId}
            flowId={flow.id}
            state={life.state}
            current={life.version}
            canPublish={canAdmin && !archived}
            publishing={inFlight}
            acting={acting}
            onPublish={openPublish}
            onView={view}
          />
        </div>
      ) : null}

      {tab === "enrollments" ? (
        <Inspector
          workspaceId={workspaceId}
          flowId={flow.id}
          state={life.state}
          current={life.version}
          canAdmin={canAdmin}
          assistantOn={assistantOn}
          onAssistantGone={reloadAssistant}
          names={names}
          catalog={catalog}
          announce={announce}
          onConflict={() => adoptCurrent(false)}
        />
      ) : null}

      {tab === "versions" && viewing !== null ? (
        <>
          <div className="fl-viewing">
            <span>Viewing version {viewing.version} · read-only</span>
            <Button onClick={() => setViewing(null)}>All versions</Button>
            <Button onClick={() => chooseTab("canvas")}>Back to the draft</Button>
          </div>
          <VersionCanvas
            key={viewing.version}
            workspaceId={workspaceId}
            flowId={flow.id}
            version={viewing}
            names={names}
            catalog={catalog}
            assistantOn={assistantOn}
            onAssistantGone={reloadAssistant}
          />
        </>
      ) : null}
      </div>

      <div className="visually-hidden" role="status" aria-live="polite">
        {announcement}
      </div>

      {archiving ? (
        <Dialog title={`Archive ${draft.name}?`} onClose={() => setArchiving(false)}>
          <p className="muted">
            Archiving is permanent. The flow cannot be published again, and no one new will enter it. Everyone still in the flow
            leaves it, whichever step they are on. The versions and their history stay readable.
          </p>
          <div className="dialog-actions">
            <Button onClick={() => setArchiving(false)}>Cancel</Button>
            <Button variant="primary" disabled={acting} aria-disabled={inFlight || undefined} onClick={() => void lifecycle("archive")}>
              Archive flow
            </Button>
          </div>
        </Dialog>
      ) : null}

      {publishOpen ? (
        <PublishDialog
          workspaceId={workspaceId}
          flowId={flow.id}
          flowName={draft.name}
          graph={draft.graph}
          names={names}
          saveNow={store.saveNow}
          itemsFor={itemsFor}
          onPick={pickFromDialog}
          publishing={inFlight}
          onPublish={() => void startPublish()}
          refusal={refusal}
          refusedIssues={refusedIssues}
          onClose={() => setPublishOpen(false)}
        />
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

      {replacing !== null ? (
        <Dialog title="Replace your unsaved changes?" onClose={() => answerReplace(false)}>
          <p className="muted">
            The canvas has changes that are not saved yet. Accepting the assistant&rsquo;s draft replaces the steps and settings with
            its own, and those changes are lost. The draft is saved, and nothing is published.
          </p>
          <div className="dialog-actions">
            <Button onClick={() => answerReplace(false)}>Keep my changes</Button>
            <Button variant="primary" onClick={() => answerReplace(true)}>
              Replace with the draft
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
function NameField({ name, onCommit, disabled }: { name: string; onCommit: (name: string) => void; disabled: boolean }) {
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
        disabled={disabled}
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
