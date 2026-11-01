import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import { Link, useParams, useSearchParams } from "react-router";

import { ApiError, client, describeError, unwrap, useResource, type components, type ErrorSummary } from "../../api";
import { Button, Dialog, EmptyState, ErrorState, LoadingState, Panel, plural } from "../../ui";
import { useAssistantAccess } from "../assistant";
import { useCatalog, useLeaveGuard } from "../segments";
import { useFlowAccess } from "./access";
import { Canvas, type FocusRequest, type IssueItem } from "./Canvas";
import { wordsFor, triggerWords } from "./describe";
import { StatePill } from "./FlowsPage";
import { placeIssues, SECTIONS, type Section } from "./issues";
import {
  insertNode,
  portsOf,
  sameJson,
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
import { ResultsView } from "./ResultsView";
import { StepPalette } from "./StepPalette";
import { useVersionCounts } from "./counts";
import { pathOverlay, type Overlay } from "./overlay";
import { StepInspector } from "./StepInspector";
import { TestView } from "./TestView";
import type { TestResult } from "./testMode";
import { useFlowDraft } from "./useFlowDraft";
import { WarmupLine, warmupProgress, type Warmup } from "./Warmup";

/** The settings section an issue about no step or connection belongs to, if any. */
function sectionOfIssue(issue: FlowIssue) {
  const found = placeIssues([issue]).bySection;
  return SECTIONS.find((section) => found.get(section.id)?.length);
}

type FlowBody = components["schemas"]["FlowBody"];

/** How often the counts of a running warm-up are read again. */
const WARMUP_POLL_MS = 5000;

const VIEWS = [
  { id: "build", label: "Build" },
  { id: "test", label: "Test" },
  { id: "results", label: "Results" },
] as const;

type ViewId = (typeof VIEWS)[number]["id"];

function viewFrom(params: URLSearchParams): ViewId {
  const value = params.get("view") ?? params.get("tab");
  return value === "test" ? "test" : value === "results" || value === "enrollments" ? "results" : "build";
}

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
  return <Editor workspaceId={workspaceId} flow={flow.data} canWrite={access.canWrite}
      canPublish={access.canPublish}
      canAdmin={access.canAdmin}
      canTestWithPerson={access.canTestWithPerson}
      canViewPii={access.canViewPii}
    />;
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
  canPublish,
  canAdmin,
  canTestWithPerson,
  canViewPii,
}: {
  workspaceId: string;
  flow: FlowDetail;
  canWrite: boolean;
  canPublish: boolean;
  canAdmin: boolean;
  canTestWithPerson: boolean;
  canViewPii: boolean;
}) {
  const [life, setLife] = useState<Life>({ state: flow.state, version: flow.current_version ?? null });
  const [warmup, setWarmup] = useState<Warmup>(flow.warmup);
  const [warmupBusy, setWarmupBusy] = useState(false);
  const [clock, setClock] = useState(() => Date.now());
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

  const [searchParams, setSearchParams] = useSearchParams();
  const activeView = viewFrom(searchParams);
  const testing = activeView === "test";
  const versionsOpen = searchParams.get("panel") === "versions" || searchParams.get("tab") === "versions";
  const [showCounts, setShowCounts] = useState(false);
  const [selected, setSelected] = useState<string | null>(null);
  const counts = useVersionCounts(workspaceId, flow.id, !versionsOpen && activeView === "build" && (showCounts || selected !== null) ? life.version : null);
  const versions = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/flows/{id}/versions", { params: { path: { ws: workspaceId, id: flow.id } } })),
    [workspaceId, flow.id, life.version],
  );
  const currentVersion = versions.status === "ready" ? versions.data.find((entry) => entry.version === life.version) : undefined;
  const draftChanged = currentVersion !== undefined &&
    (!sameJson(currentVersion.graph, draft.graph) || !sameJson(currentVersion.settings, draft.settings));
  const countsOverlay = useMemo<Overlay | undefined>(() => {
    if (!showCounts || counts.status !== "ready" || !counts.data) return undefined;
    return { counts: new Map(counts.data.nodes.map((entry) => [entry.node_id, entry])) };
  }, [showCounts, counts]);
  const publishBlocked = (store.issues?.length ?? 0) > 0;
  const [inspectorFocus, setInspectorFocus] = useState(true);
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
  const [testResult, setTestResult] = useState<TestResult | null>(null);
  const [assistantOpen, setAssistantOpen] = useState(false);
  const [previousView, setPreviousView] = useState(activeView);
  /** Bumped when a whole new draft is loaded, so the canvas draws it afresh. */
  const [layoutEpoch, setLayoutEpoch] = useState(0);
  /** The steps the assistant's explanation cites, marked on the canvas while it is open. */
  const [cited, setCited] = useState<readonly string[]>([]);
  if (previousView !== activeView) {
    setPreviousView(activeView);
    setTestResult(null);
    setAssistantOpen(false);
    setCited([]);
  }
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
    chooseView("build");
    setSelected(id);
    setSettingsFocus(null);
    setInspectorFocus(takeFocus);
    if (!takeFocus) setFocusRequest({ id, nonce: nonce.current });
  }

  /** Moves focus to a step without changing the inspector, so the assistant's sheet stays. */
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
    setSelected(TRIGGER_ID);
    chooseView("build");
    setSettingsFocus({ section, nonce: nonce.current });
  }

  function open(id: string) {
    if (id === TRIGGER_ID) openSettings("trigger");
    else focusStep(id, true);
  }

  function closeInspector() {
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

  /** After an action the button that had focus may be gone, so focus goes to the first action or the view in use. */
  function refocusActions() {
    if (!mounted.current) return;
    setTimeout(() => {
      const page = pageRef.current;
      if (!mounted.current || !page) return;
      (page.querySelector<HTMLElement>("#flow-primary-action:not(:disabled)") ?? page.querySelector<HTMLElement>(`#flow-view-${activeView}`))?.focus();
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

  /** Shows the warm-up a response carries, and measures the window from this moment. */
  function adoptWarmup(next: Warmup) {
    setWarmup(next);
    setClock(Date.now());
  }

  async function changeWarmup(request: () => Promise<{ warmup: Warmup }>, said: string) {
    if (warmupBusy) return;
    setWarmupBusy(true);
    setNotice(null);
    try {
      const updated = await request();
      if (!mounted.current) return;
      adoptWarmup(updated.warmup);
      announce(said);
    } catch (error) {
      if (mounted.current) noticeOf(error);
    } finally {
      if (mounted.current) setWarmupBusy(false);
    }
  }

  function saveWarmup(enabled: boolean, seconds: number) {
    const path = { params: { path: { ws: workspaceId, id: flow.id } } };
    void changeWarmup(
      () => unwrap(client.PUT("/api/v1/workspaces/{ws}/flows/{id}/warmup", { ...path, body: { enabled, seconds } })),
      "The warm-up is saved.",
    );
  }

  function openWarmup() {
    const path = { params: { path: { ws: workspaceId, id: flow.id } } };
    void changeWarmup(() => unwrap(client.POST("/api/v1/workspaces/{ws}/flows/{id}/warmup/open", path)), "The flow is open.");
  }

  const warmupState = life.state === "published" ? warmupProgress(warmup, clock) : null;
  const warming = warmupState !== null;
  useEffect(() => {
    if (!warming) return;
    // The clock moves and the counts are read again while the window runs.
    const timer = setInterval(() => {
      setClock(Date.now());
      void unwrap(client.GET("/api/v1/workspaces/{ws}/flows/{id}", { params: { path: { ws: workspaceId, id: flow.id } } }))
        .then((fresh) => {
          if (mounted.current) setWarmup(fresh.warmup);
        })
        .catch(() => undefined);
    }, WARMUP_POLL_MS);
    return () => clearInterval(timer);
  }, [warming, workspaceId, flow.id]);

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
      adoptWarmup(updated.warmup);
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
    if (inFlight || acting || busy || publishBlocked) return;
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

  function viewVersion(version: FlowVersion) {
    setViewing(version);
    announce(`Viewing version ${version.version}, read-only.`);
  }

  const editGraph = (change: (graph: Graph) => Graph, key?: string) =>
    store.edit((current) => ({ ...current, graph: change(current.graph) }), key);
  const editSettings = (change: (settings: Settings) => Settings, key?: string) =>
    store.edit((current) => ({ ...current, settings: change(current.settings) }), key);

  function add(type: NodeType) {
    const { id } = insertNode(draft.graph, type, selected);
    editGraph((graph) => insertNode(graph, type, selected).graph);
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

  const selectedNode = selected === null ? undefined : draft.graph.nodes.find((candidate) => candidate.id === selected);
  const doomed = deleting === null ? undefined : draft.graph.nodes.find((candidate) => candidate.id === deleting);

  function paramsFor(next: ViewId) {
    const params = new URLSearchParams(searchParams);
    params.delete("tab");
    params.delete("panel");
    if (next === "build") params.delete("view");
    else params.set("view", next);
    return params;
  }

  function chooseView(next: ViewId) {
    if (next !== activeView || versionsOpen) {
      setViewing(null);
      setCited([]);
      setTestResult(null);
      setAssistantOpen(false);
      setFocusRequest(null);
      setSettingsFocus(null);
      setInspectorFocus(false);
      setSearchParams(paramsFor(next));
    }
  }

  function onViewKey(event: KeyboardEvent<HTMLElement>) {
    const at = VIEWS.findIndex((entry) => entry.id === activeView);
    const index = event.key === "ArrowRight" ? (at + 1) % VIEWS.length
      : event.key === "ArrowLeft" ? (at + VIEWS.length - 1) % VIEWS.length
      : event.key === "Home" ? 0 : event.key === "End" ? VIEWS.length - 1 : null;
    if (index === null) return;
    event.preventDefault();
    const next = VIEWS[index];
    if (next) {
      chooseView(next.id);
      document.getElementById(`flow-view-${next.id}`)?.focus();
    }
  }

  function toggleVersions() {
    setViewing(null);
    const params = paramsFor("build");
    if (!versionsOpen) params.set("panel", "versions");
    setSearchParams(params);
  }

  function portName(id: string, port: string): string | null {
    const node = draft.graph.nodes.find((candidate) => candidate.id === id);
    return node ? portLabel(node, port) : null;
  }

  const version = stateLabel(life.state, life.version);
  const selectedPort = selectedNode ? portsOf(selectedNode)[0] : undefined;
  const canvas = (
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
      overlay={testing ? testOverlay : citedOverlay ?? countsOverlay}
      assistant={assistantOn && !testing ? { open: assistantOpen, onToggle: toggleAssistant } : undefined}
      selectedId={testing ? null : selected}
      focusRequest={focusRequest}
      canUndo={store.canUndo}
      onOpen={testing ? () => undefined : open}
      onClose={closeInspector}
      onWire={wireStep}
      onUndo={store.undo}
      onDeleteRequest={(id) => setDeleting(id)}
      onPickIssue={pick}
    >
      {assistantOn && !testing && assistantOpen ? (
        <div className="fl-sheet" hidden={selectedNode !== undefined}>
          <FlowAssistantSheet
            workspaceId={workspaceId} flowId={flow.id} version={null}
            graph={draft.graph} revision={[draft.graph, draft.settings]} names={names}
            onGone={reloadAssistant} busy={busy}
            drafting={editable ? { confirmAccept: confirmAssistantAccept, onAccept: loadAssistantDraft, onAcceptFailed: reconcileAccept } : undefined}
            prepare={editable ? store.saveNow : undefined}
            onReveal={revealStep} onCited={setCited} onClose={toggleAssistant}
          />
        </div>
      ) : null}
    </Canvas>
  );

  return (
    <div className="fl-page" ref={pageRef} aria-busy={busy || undefined}>
      <div className="fl-head">
        <nav aria-label="Breadcrumb"><Link to={`/w/${workspaceId}/flows`}>Flows</Link></nav>
        {editable ? (
          <NameField disabled={busy} name={draft.name} onCommit={(name) => store.edit((current) => ({ ...current, name }), "name")} />
        ) : (
          <h1 className="fl-name-read">{draft.name}</h1>
        )}
        <span className="fl-state">
          <StatePill state={life.state} />
          {version ? <span className="faint">{version}</span> : null}
          {draftChanged ? <span className="pill">Draft with changes</span> : null}
        </span>
        <nav className="fl-views" aria-label="Flow views" onKeyDown={onViewKey}>
          {VIEWS.map((entry) => (
            <Link key={entry.id} id={`flow-view-${entry.id}`} className="fl-view"
              to={{ search: paramsFor(entry.id).toString() }}
              aria-current={activeView === entry.id ? "page" : undefined}
              onClick={(event) => {
                if (event.button === 0 && !event.metaKey && !event.ctrlKey && !event.shiftKey && !event.altKey) {
                  event.preventDefault();
                  chooseView(entry.id);
                }
              }}>
              {entry.label}
            </Link>
          ))}
        </nav>
        <Button aria-expanded={versionsOpen} aria-controls={versionsOpen ? "flow-versions" : undefined} onClick={toggleVersions}>Versions</Button>
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
        {canPublish && life.state !== "archived" ? (
          <div className="fl-actions">
            <Button id="flow-primary-action" variant="primary" disabled={acting || busy} aria-disabled={inFlight || publishBlocked || undefined}
              aria-describedby={publishBlocked ? "flow-publish-blocked" : undefined} onClick={openPublish}>
              {inFlight ? "Publishing…" : store.issues === null ? "Review and publish" : `Review and publish (${issueItems.length})`}
            </Button>
            {publishBlocked ? (
              <span id="flow-publish-blocked" className="visually-hidden">
                {`${plural(issueItems.length, "problem", "problems")} must be fixed before this flow can be published.`}
              </span>
            ) : null}
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

      {warmupState ? (
        <WarmupLine warmup={warmup} progress={warmupState} canOpen={canAdmin} busy={warmupBusy} onOpen={openWarmup} />
      ) : null}

      {notice ? (
        <p className="fl-notice" role="alert">
          {notice}
        </p>
      ) : null}

      <div id="flow-panel" role="region" aria-label={versionsOpen ? "Versions" : VIEWS.find((entry) => entry.id === activeView)?.label} className="fl-panel">
        {versionsOpen ? (
          <section id="flow-versions" className="fl-version-panel" aria-label="Published versions">
            <div className="fl-viewing">
              <h2>{viewing ? `Viewing version ${viewing.version} · read-only` : "Versions"}</h2>
              {viewing ? <Button onClick={() => setViewing(null)}>All versions</Button> : null}
              <Button onClick={() => chooseView("build")}>Back to the draft</Button>
            </div>
            {viewing ? (
              <VersionCanvas key={viewing.version} workspaceId={workspaceId} flowId={flow.id} version={viewing}
                names={names} catalog={catalog} assistantOn={assistantOn} onAssistantGone={reloadAssistant} />
            ) : (
              <div className="fl-pane">
                <VersionsView workspaceId={workspaceId} versions={versions} state={life.state} current={life.version}
                  canPublish={canPublish && !archived} publishing={inFlight} acting={acting || busy || publishBlocked}
                  onPublish={openPublish} onView={viewVersion} />
              </div>
            )}
          </section>
        ) : activeView === "results" ? (
          <ResultsView workspaceId={workspaceId} flowId={flow.id} versions={versions} state={life.state} current={life.version}
            canPublish={canPublish} canSeePeople={canViewPii} assistantOn={assistantOn} onAssistantGone={reloadAssistant}
            names={names} announce={announce} onConflict={() => adoptCurrent(false)} />
        ) : testing ? (
          canWrite ? (
            <TestView workspaceId={workspaceId} flowId={flow.id} canSeePeople={canTestWithPerson}
              describeStep={describeStep} portName={portName} graph={draft.graph} settingsUnsaved={store.settingsUnsaved}
              onResult={setTestResult} announce={announce}>
              {canvas}
            </TestView>
          ) : <EmptyState title="Testing needs an editor">You can explore the flow in Build and read its Results.</EmptyState>
        ) : (
          <>
            <div className="fl-build-bar">
              {life.version !== null ? (
                <label className="check">
                  <input type="checkbox" checked={showCounts} onChange={(event) => setShowCounts(event.target.checked)} />
                  Show counts from version {life.version}
                </label>
              ) : <span className="faint">Draft · results are available after publishing.</span>}
              {showCounts && counts.status === "loading" ? <span role="status">Loading the counts</span> : null}
              {showCounts && counts.status === "error" ? (
                <span role="alert">{describeError(counts.error).title} <Button onClick={counts.reload}>Retry counts</Button></span>
              ) : null}
              {draft.graph.nodes.length === 0 ? <span className="muted">Start with the trigger, then add your first step.</span> : null}
            </div>
            <div className="fl-workbench">
              {editable ? <StepPalette disabled={!canEdit} onAdd={add}
                placement={selectedNode
                  ? selectedPort
                    ? `Inserts after this step via ${portLabel(selectedNode, selectedPort)}. Other outputs need connecting.`
                    : "This step has no output. New steps start disconnected."
                  : selected === TRIGGER_ID ? "Inserts directly after the trigger." : "Select a step to insert after it, or add a disconnected step."} /> : null}
              {canvas}
              {selectedNode ? (
                <StepInspector key={selectedNode.id} workspaceId={workspaceId} graph={draft.graph}
                  node={selectedNode} names={names} steps={steps} issues={issuesOfStep(selectedNode.id)}
                  catalog={catalog} readOnly={!canEdit} autoFocus={inspectorFocus} counts={counts}
                  onClose={closeInspector}
                  onChange={(next: FlowNode, key) => editGraph((graph) => replaceNode(graph, next), key)}
                  onRemovePort={(next, prefix, index) => editGraph((graph) => removeIndexedPort(graph, next, prefix, index))}
                  onWire={(port, to) => wireStep(selectedNode.id, port, to)}
                  onDelete={() => setDeleting(selectedNode.id)} />
              ) : (
                <section className="fl-inspector stack" aria-label="Flow settings">
                  <h2>Flow settings</h2>
                  <p className="faint">Starts with: {triggerWords(draft.settings, names).title}.</p>
                  <SettingsView workspaceId={workspaceId} settings={draft.settings} catalog={catalog}
                    readOnly={!canEdit} issues={placed.bySection} focus={settingsFocus} onChange={editSettings}
                    warmup={{ value: warmup, canChange: canPublish && !archived, busy: warmupBusy, onSave: saveWarmup }} />
                </section>
              )}
            </div>
          </>
        )}
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
