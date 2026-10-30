import { useEffect, useMemo, useRef, useState } from "react";
import { Link } from "react-router";

import { ApiError, client, describeError, unwrap, useResource, type components, type Resource } from "../../api";
import {
  Button,
  Dialog,
  EmptyState,
  formatDateTime,
  InlineError,
  Pill,
  reasonLabel,
  ResourceView,
  SelectField,
  shortId,
  type Tone,
} from "../../ui";

import { TraceTree, traceHref, type TraceTreeRecord } from "../traces";
import { Canvas } from "./Canvas";
import { useVersionCounts, type VersionCounts } from "./counts";
import { wordsFor, type Names } from "./describe";
import { placeIssues } from "./issues";
import { TraceExplanation } from "./TraceExplanation";
import { kindLabel, TRIGGER_ID, type FlowDetail, type Graph } from "./model";
import { pathOverlay, type NodeMark, type NodeTally, type Overlay } from "./overlay";
import { PersonPicker, type PersonChoice } from "./TestView";
import type { FlowVersion } from "./VersionsView";

type EnrollmentItem = components["schemas"]["EnrollmentItemBody"];
type EnrollmentPath = components["schemas"]["EnrollmentPathBody"];
type NodeCount = components["schemas"]["NodeCountBody"];

const STATES = ["active", "waiting", "held", "completed", "exited", "failed"] as const;
const PAGE = 25;
const CHANGED_ELSEWHERE = "This changed somewhere else. It is shown as it is now; nothing was done.";

interface Props {
  workspaceId: string;
  flowId: string;
  versions: Resource<FlowVersion[]>;
  state: FlowDetail["state"];
  /** The newest published version of the flow. */
  current: number | null;
  canPublish: boolean;
  /** Looking a person up by an identifier needs the personal data grant. */
  canSeePeople: boolean;
  /** Whether the assistant is on and the person may use it: a trace can then be explained. */
  assistantOn: boolean;
  /** The assistant refused a request because it is off or no longer the person's to use. */
  onAssistantGone: () => void;
  names: Names;
  announce: (text: string) => void;
  /** Reads the flow's lifecycle state again after a 409, so the editor shows what the API now says. */
  onConflict: () => Promise<void>;
}

/** Version totals, step results and the enrollments behind them. */
export function ResultsView(props: Props) {
  const { versions, ...rest } = props;
  return (
    <ResourceView
      resource={versions}
      loadingLabel="Loading the versions"
      isEmpty={(list) => list.length === 0}
      empty={
        <div className="fl-pane">
          <EmptyState title="No one can be in this flow yet">
            Publish a version first. People enter a published version, and they are listed here with the steps they have reached.
          </EmptyState>
        </div>
      }
    >
      {(list) => <ResultsBody {...rest} versions={list} />}
    </ResourceView>
  );
}

function stepTitle(graph: Graph | undefined, names: Names, id: string): string {
  if (id === TRIGGER_ID) return "Trigger";
  const node = graph?.nodes.find((candidate) => candidate.id === id);
  if (!node) return id === "settings.goal" ? "Goal setting" : id;
  return `${kindLabel(node.type)}: ${wordsFor(node, names).title}`;
}

const STATE_TONE: Record<string, Tone> = { failed: "error", held: "attention" };

interface Filters {
  state: string;
  version: string;
  node: string;
}

function ResultsBody({ versions, ...props }: Omit<Props, "versions"> & { versions: FlowVersion[] }) {
  const { workspaceId, flowId, names, current } = props;
  const sorted = useMemo(() => [...versions].sort((left, right) => right.version - left.version), [versions]);
  const newest = sorted[0]?.version ?? 1;
  const [version, setVersion] = useState<number>(current ?? newest);
  const [showCounts, setShowCounts] = useState(true);
  const [person, setPerson] = useState<PersonChoice | null>(null);
  const enrollmentPanel = useRef<HTMLElement>(null);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  useEffect(() => {
    if (selectedId !== null) enrollmentPanel.current?.focus();
  }, [selectedId]);
  const [filters, setFilters] = useState<Filters>({ state: "", version: "", node: "" });
  /** Bumped when the list must be read again; the counts and the detail reload in place, so what is shown stays. */
  const [refresh, setRefresh] = useState(0);
  /** An action has finished, so focus goes back once the detail has been read again. */
  const [returnFocus, setReturnFocus] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [acting, setActing] = useState(false);
  const [confirm, setConfirm] = useState<"skip" | "exit" | null>(null);

  const graphs = useMemo(() => new Map(sorted.map((entry) => [entry.version, entry])), [sorted]);
  const shown = graphs.get(version) ?? sorted[0];
  const none = useMemo(() => placeIssues([]), []);

  const counts = useVersionCounts(workspaceId, flowId, version);
  const detail = useResource(async (): Promise<EnrollmentPath | null> => {
    if (selectedId === null) return null;
    return unwrap(
      client.GET("/api/v1/workspaces/{ws}/flows/{id}/enrollments/{enrollment_id}", {
        params: { path: { ws: workspaceId, id: flowId, enrollment_id: selectedId } },
      }),
    );
  }, [workspaceId, flowId, selectedId]);

  function reread() {
    setRefresh((value) => value + 1);
    counts.reload();
    detail.reload();
  }

  const tallies = counts.status === "ready" ? (counts.data?.nodes ?? null) : null;

  const picked = detail.status === "ready" ? detail.data : null;

  const overlay = useMemo<Overlay | undefined>(() => {
    if (!shown) return undefined;
    const out: {
      -readonly [K in keyof Overlay]: Overlay[K];
    } = {};
    if (showCounts && tallies) {
      const byNode = new Map<string, NodeTally>(tallies.map((entry) => [entry.node_id, entry]));
      out.counts = byNode;
    }
    if (picked && picked.enrollment.version === shown.version) {
      Object.assign(
        out,
        pathOverlay(
          shown.graph,
          picked.steps.map((step) => ({ node: step.node_id })),
        ),
      );
      out.marks = marksOf(picked);
    }
    return out;
  }, [shown, showCounts, tallies, picked]);

  if (!shown) return null;

  function select(item: EnrollmentItem) {
    setSelectedId(item.id);
    setReturnFocus(false);
    setVersion(item.version);
    props.announce(`Showing the path of one enrollment in version ${item.version}.`);
  }

  function back() {
    setSelectedId(null);
    setReturnFocus(false);
    props.announce("Back to all enrollments.");
  }

  /**
   * A 409: the flow's state is read again, and the notice names the cause the problem's code gives.
   * Only a conflict with no such cause says the enrollment changed somewhere else.
   */
  async function conflict(error: ApiError, kind: "retry" | "skip" | "exit") {
    const done = kind === "retry" ? "retried" : kind === "skip" ? "skipped" : "ended";
    const said =
      error.code === "flow_paused"
        ? `The flow is paused, so nothing was ${done}. Resume it first.`
        : error.code === "flow_archived"
          ? `The flow is archived, so nothing was ${done}.`
          : CHANGED_ELSEWHERE;
    setNotice(said);
    props.announce(said);
    reread();
    await props.onConflict();
  }

  async function act(kind: "retry" | "skip" | "exit", enrollmentId?: string) {
    if (acting) return;
    setActing(true);
    setNotice(null);
    try {
      if (kind === "exit") {
        await unwrap(
          client.POST("/api/v1/workspaces/{ws}/flows/{id}/versions/{n}/exit-enrollments", {
            params: { path: { ws: workspaceId, id: flowId, n: version } },
          }),
        );
        props.announce(`Everyone still in version ${version} is being ended.`);
      } else if (enrollmentId !== undefined) {
        const params = { params: { path: { ws: workspaceId, id: flowId, enrollment_id: enrollmentId } } };
        await unwrap(
          kind === "retry"
            ? client.POST("/api/v1/workspaces/{ws}/flows/{id}/enrollments/{enrollment_id}/retry", params)
            : client.POST("/api/v1/workspaces/{ws}/flows/{id}/enrollments/{enrollment_id}/skip", params),
        );
        props.announce(kind === "retry" ? "The step will run again." : "The step is skipped.");
      }
      reread();
    } catch (error) {
      if (error instanceof ApiError && error.status === 409) {
        await conflict(error, kind);
      } else {
        const summary = describeError(error);
        setNotice(summary.detail || summary.title);
      }
    } finally {
      setActing(false);
      setConfirm(null);
      // Only Retry and Skip act on the open detail; a bulk exit has nothing to return focus to.
      if (kind !== "exit") setReturnFocus(true);
    }
  }

  const older = props.current !== null && version < props.current;
  const waitingHere = tallies?.reduce((sum, entry) => sum + entry.waiting, 0) ?? null;
  const exitReason =
    props.state !== "published" ? "A paused or archived flow cannot end anyone's enrollment." : null;
  const stepReason =
    props.state === "paused"
      ? "The flow is paused, so a failed step cannot be retried or skipped. Resume it first."
      : props.state === "archived"
        ? "The flow is archived, so a failed step cannot be retried or skipped."
        : null;

  return (
    <>
      {notice ? (
        <p className="fl-notice" role="alert">
          {notice}
        </p>
      ) : null}
      <div className="fl-results stack">
        <div className="fl-results-summary stack" role="group" aria-label="Counts and versions">
          <SelectField
            label="Counts for"
            value={String(version)}
            onChange={(event) => {
              setVersion(Number(event.target.value));
              setSelectedId(null);
              setReturnFocus(false);
              setFilters((value) => ({ ...value, node: "" }));
            }}
          >
            {sorted.map((entry) => (
              <option key={entry.version} value={entry.version}>
                {`v${entry.version}${entry.version === props.current ? " · current" : ""}`}
              </option>
            ))}
          </SelectField>
          <div className="row">
            <Button aria-pressed={showCounts} onClick={() => setShowCounts((value) => !value)}>
              Counts on canvas
            </Button>
          </div>
          <p className="faint">
            Everyone who entered version {version}, computed when you opened this. Dark badges count step runs (a retried step counts
            again), light ones the people waiting, red ones failures.
          </p>
          <Tally counts={counts} graph={shown.graph} names={names} />
          {older ? (
            <div className="stack-tight" role="group" aria-label="People still on this version">
              <span>
                {waitingHere === null ? "Counting who is still in this version…" : `${waitingHere} waiting in version ${version}, which is no longer the current one.`}
              </span>
              {props.canPublish ? (
                <>
                  <Button
                    aria-disabled={acting || exitReason !== null || undefined}
                    onClick={() => {
                      if (!acting && exitReason === null) setConfirm("exit");
                    }}
                  >
                    Exit them…
                  </Button>
                  {exitReason ? <span className="faint">{exitReason}</span> : null}
                </>
              ) : (
                <span className="faint">Ending their enrollments needs a publisher.</span>
              )}
            </div>
          ) : null}
        </div>

        {props.canSeePeople ? (
          <section className="panel panel-pad stack" aria-label="Person lookup">
            <h2>Why did or didn’t someone get it?</h2>
            <PersonPicker workspaceId={workspaceId} chosen={person} onChoose={setPerson} />
            {person ? <Link to={`/w/${workspaceId}/people/${person.id}?tab=messages`}>Messages for {person.label}</Link> : null}
          </section>
        ) : null}
        <div className="fl-results-enrollments">
          <Canvas
            fitKey={shown.version}
            graph={shown.graph}
            settings={shown.settings}
            names={names}
            placed={none}
            issueItems={[]}
            checked={false}
            checking={false}
            editable={false}
            selectedId={null}
            focusRequest={null}
            canUndo={false}
            overlay={overlay}
            onOpen={(id) => {
              if (id === TRIGGER_ID) return;
              setFilters((value) => ({ ...value, node: id }));
              setSelectedId(null);
              setReturnFocus(false);
              props.announce(`Showing the enrollments at ${stepTitle(shown.graph, names, id)}.`);
            }}
            onClose={() => undefined}
            onWire={() => undefined}
            onUndo={() => undefined}
            onDeleteRequest={() => undefined}
            onPickIssue={() => undefined}
          />

          <section ref={enrollmentPanel} tabIndex={-1} className="fl-enrollments stack"
            onKeyDown={(event) => {
              if (event.key === "Escape" && selectedId !== null) { event.stopPropagation(); back(); }
            }}
            aria-label={selectedId === null ? "Enrollments" : "One enrollment"}>
            <div className="row">
              <h2>{selectedId === null ? "Enrollments" : "One enrollment"}</h2>
              {selectedId !== null ? <Button onClick={back}>Back to all</Button> : null}
            </div>
            {selectedId === null ? (
              <EnrollmentList
                key={`${filters.state}|${filters.version}|${filters.node}|${refresh}`}
                workspaceId={workspaceId}
                flowId={flowId}
                filters={filters}
                onFilters={setFilters}
                sorted={sorted}
                graph={shown.graph}
                graphs={graphs}
                names={names}
                onSelect={select}
              />
            ) : (
              <ResourceView resource={detail} loadingLabel="Loading the enrollment" isEmpty={(data) => data === null}>
                {(data) =>
                  data ? (
                    <EnrollmentDetail
                      key={data.enrollment.id}
                      path={data}
                      workspaceId={workspaceId}
                      graph={graphs.get(data.enrollment.version)?.graph}
                      names={names}
                      canPublish={props.canPublish}
                      assistantOn={props.assistantOn}
                      onAssistantGone={props.onAssistantGone}
                      acting={acting}
                      reason={stepReason}
                      refreshing={detail.status === "ready" && detail.refreshing}
                      returnFocus={returnFocus}
                      onFocused={() => setReturnFocus(false)}
                      onRetry={() => !stepReason && void act("retry", data.enrollment.id)}
                      onSkip={() => !acting && !stepReason && setConfirm("skip")}
                    />
                  ) : null
                }
              </ResourceView>
            )}

          </section>
        </div>
      </div>
      {confirm === "skip" && selectedId !== null ? (
        <Dialog title="Skip the failed step?" onClose={() => setConfirm(null)}>
          <p className="muted">
            The step is not run. The person carries on from the step after it, and the skip is recorded on their trace.
          </p>
          <div className="dialog-actions">
            <Button onClick={() => setConfirm(null)}>Cancel</Button>
            <Button variant="primary" aria-disabled={acting || undefined} onClick={() => void act("skip", selectedId)}>
              Skip the step
            </Button>
          </div>
        </Dialog>
      ) : null}
      {confirm === "exit" ? (
        <Dialog title={`End everyone still in version ${version}?`} onClose={() => setConfirm(null)}>
          <p className="muted">
            Every enrollment of version {version} that has not finished leaves the flow, whichever step it is on. Each is recorded on its own
            trace. This cannot be undone.
          </p>
          <div className="dialog-actions">
            <Button onClick={() => setConfirm(null)}>Cancel</Button>
            <Button variant="primary" aria-disabled={acting || undefined} onClick={() => void act("exit")}>
              End their enrollments
            </Button>
          </div>
        </Dialog>
      ) : null}
    </>
  );
}

/** The delivery counts of a send step as words; “None” for a step that sends nothing. */
function deliveryText(entry: NodeCount): string {
  const parts: string[] = [];
  if (entry.sent > 0) parts.push(`${entry.sent} sent`);
  for (const skip of entry.skipped) parts.push(`${skip.count} skipped: ${reasonLabel(skip.reason)}`);
  if (entry.send_failed > 0) parts.push(`${entry.send_failed} send failed`);
  return parts.length === 0 ? "None" : parts.join("; ");
}

/** The counts as text, for those who do not read them off the canvas. */
function Tally({
  counts,
  graph,
  names,
}: {
  counts: Resource<VersionCounts | null>;
  graph: Graph;
  names: Names;
}) {
  if (counts.status === "loading") {
    return (
      <p className="muted" role="status">
        Loading the counts
      </p>
    );
  }
  if (counts.status === "error") {
    return (
      <div className="stack">
        <InlineError {...describeError(counts.error)} />
        <Button onClick={counts.reload}>Try again</Button>
      </div>
    );
  }
  if (!counts.data) return null;
  return (
    <>
      <dl className="fl-totals" aria-label={`Totals for version ${counts.data.version}`}>
        {[
          { label: "Step runs", value: counts.data.nodes.filter((node) => node.node_type !== "settings.goal").reduce((sum, node) => sum + node.entered, 0) },
          { label: "Waiting", value: counts.data.nodes.reduce((sum, node) => sum + node.waiting, 0) },
          { label: "Messages sent", value: counts.data.nodes.reduce((sum, node) => sum + node.sent, 0) },
          { label: "Goal conversions", value: counts.data.nodes.reduce((sum, node) => sum + node.goal_conversions, 0) },
        ].map(({ label, value }) => <div key={label}><dt>{label}</dt><dd>{value.toLocaleString("en")}</dd></div>)}
      </dl>
      {counts.data.nodes.every((node) => node.entered === 0 && node.waiting === 0 && node.failed === 0 && node.sent === 0 && node.send_failed === 0 && node.skipped.length === 0) ? (
        <EmptyState title="No results yet">No steps have run in version {counts.data.version}.</EmptyState>
      ) : null}
      <h2>Step by step · version {counts.data.version}</h2>
      <div className="fl-table-scroll">
        <table className="table">
          <caption className="visually-hidden">Counts per step of version {counts.data.version}</caption>
          <thead>
            <tr>
              <th scope="col">Step</th>
              <th scope="col">Reached (step runs)</th>
              <th scope="col">Waiting</th>
              <th scope="col">Failed step runs</th>
              <th scope="col">Skipped messages</th>
              <th scope="col">Delivery</th>
            </tr>
          </thead>
          <tbody>
            {counts.data.nodes.map((entry: NodeCount) => (
              <tr key={entry.node_id}>
                <th scope="row">{stepTitle(graph, names, entry.node_id)}</th>
                <td>{entry.entered}</td>
                <td>{entry.waiting}</td>
                <td>{entry.failed}</td>
                <td>{entry.skipped.reduce((sum, skip) => sum + skip.count, 0)}</td>
                <td>{deliveryText(entry)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <p className="faint">Reached counts the runs of a step, so a step that was retried counts again. It is not a count of people.</p>
    </>
  );
}

function EnrollmentList({
  workspaceId,
  flowId,
  filters,
  onFilters,
  sorted,
  graph,
  graphs,
  names,
  onSelect,
}: {
  workspaceId: string;
  flowId: string;
  filters: Filters;
  onFilters: (next: Filters) => void;
  sorted: readonly FlowVersion[];
  graph: Graph;
  graphs: ReadonlyMap<number, FlowVersion>;
  names: Names;
  onSelect: (item: EnrollmentItem) => void;
}) {
  const query = {
    limit: PAGE,
    ...(filters.state ? { state: filters.state } : {}),
    ...(filters.version ? { version: Number(filters.version) } : {}),
    ...(filters.node ? { node: filters.node } : {}),
  };
  const first = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/flows/{id}/enrollments", { params: { path: { ws: workspaceId, id: flowId }, query } })),
    [workspaceId, flowId, filters.state, filters.version, filters.node],
  );
  const [more, setMore] = useState<{ items: EnrollmentItem[]; next: string | null } | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [moreError, setMoreError] = useState<unknown>(null);

  async function loadMore(before: string) {
    if (loadingMore) return;
    setLoadingMore(true);
    setMoreError(null);
    try {
      const page = await unwrap(
        client.GET("/api/v1/workspaces/{ws}/flows/{id}/enrollments", {
          params: { path: { ws: workspaceId, id: flowId }, query: { ...query, cursor: before } },
        }),
      );
      setMore((value) => ({ items: [...(value?.items ?? []), ...page.enrollments], next: page.next_cursor ?? null }));
    } catch (error) {
      setMoreError(error);
    } finally {
      setLoadingMore(false);
    }
  }

  const filtered = filters.state !== "" || filters.version !== "" || filters.node !== "";

  return (
    <>
      <form className="fl-sheet-section" aria-label="Filter enrollments" onSubmit={(event) => event.preventDefault()}>
        <SelectField label="State" value={filters.state} onChange={(event) => onFilters({ ...filters, state: event.target.value })}>
          <option value="">Any state</option>
          {STATES.map((state) => (
            <option key={state} value={state}>
              {state}
            </option>
          ))}
        </SelectField>
        <SelectField label="Version" value={filters.version} onChange={(event) => onFilters({ ...filters, version: event.target.value })}>
          <option value="">Any version</option>
          {sorted.map((entry) => (
            <option key={entry.version} value={entry.version}>
              {`v${entry.version}`}
            </option>
          ))}
        </SelectField>
        <SelectField label="Step" value={filters.node} onChange={(event) => onFilters({ ...filters, node: event.target.value })}>
          <option value="">Any step</option>
          {graph.nodes.map((node) => (
            <option key={node.id} value={node.id}>
              {stepTitle(graph, names, node.id)}
            </option>
          ))}
        </SelectField>
      </form>
      <ResourceView
        resource={first}
        loadingLabel="Loading the enrollments"
        isEmpty={(page) => page.enrollments.length === 0}
        empty={
          <EmptyState
            title={filtered ? "No enrollments match these filters" : "No one has entered this flow yet"}
            action={filtered ? <Button onClick={() => onFilters({ state: "", version: "", node: "" })}>Clear the filters</Button> : undefined}
          >
            {filtered ? "Change a filter to see more." : "People appear here once the flow is live and they enter it."}
          </EmptyState>
        }
      >
        {(page) => {
          const items = [...page.enrollments, ...(more?.items ?? [])];
          const next = more ? more.next : (page.next_cursor ?? null);
          return (
            <>
              <ul className="fl-versions" aria-label="Enrollments">
                {items.map((item) => (
                  <li key={item.id} className="fl-version">
                    <Button
                      aria-label={`Open the enrollment of person ${shortId(item.person_id)}, ${item.state}, version ${item.version}`}
                      onClick={() => onSelect(item)}
                    >
                      Person {shortId(item.person_id)}
                    </Button>
                    <Pill tone={STATE_TONE[item.state] ?? "neutral"}>{item.state}</Pill>
                    <span className="fl-version-meta">
                      v{item.version} · at {stepTitle(graphs.get(item.version)?.graph, names, item.failed_node ?? item.node)} ·{" "}
                      <time dateTime={item.created_at}>{formatDateTime(item.created_at)}</time>
                    </span>
                  </li>
                ))}
              </ul>
              {moreError ? <InlineError {...describeError(moreError)} /> : null}
              {next !== null ? (
                <div>
                  <Button aria-disabled={loadingMore || undefined} onClick={() => void loadMore(next)}>
                    {loadingMore ? "Loading…" : "Show more"}
                  </Button>
                </div>
              ) : null}
            </>
          );
        }}
      </ResourceView>
    </>
  );
}

/** The element id of a decision's row in the trace tree, so an explanation can take the reader to it. */
function decisionRowId(recordId: string): string {
  return `decision-${recordId}`;
}

const STEP_OUTCOMES: Record<string, string> = { done: "Done", waiting: "Waiting here", failed: "Failed" };

/** The operator decisions of an enrollment by step: the steps that were retried and skipped. */
function operatorDecisions(path: EnrollmentPath, outcome: "retried" | "skipped"): Set<string> {
  const nodes = new Set<string>();
  for (const record of path.decisions) {
    const node = record.subject_ids["node"];
    if (record.action === "flow.enrollment" && record.outcome === outcome && typeof node === "string") nodes.add(node);
  }
  return nodes;
}

function marksOf(path: EnrollmentPath): Map<string, NodeMark> {
  const marks = new Map<string, NodeMark>();
  for (const node of operatorDecisions(path, "retried")) marks.set(node, { tone: "retried", text: "Retried" });
  for (const node of operatorDecisions(path, "skipped")) marks.set(node, { tone: "skipped", text: "Skipped" });
  const { enrollment } = path;
  if (enrollment.state === "failed" && enrollment.failed_node) {
    marks.set(enrollment.failed_node, { tone: "failed", text: "Failed here" });
  }
  return marks;
}

function EnrollmentDetail({
  path,
  workspaceId,
  graph,
  names,
  canPublish,
  assistantOn,
  onAssistantGone,
  acting,
  reason,
  refreshing,
  returnFocus,
  onFocused,
  onRetry,
  onSkip,
}: {
  path: EnrollmentPath;
  workspaceId: string;
  graph: Graph | undefined;
  names: Names;
  canPublish: boolean;
  assistantOn: boolean;
  onAssistantGone: () => void;
  acting: boolean;
  /** Why Retry and Skip are refused, when the flow's state forbids them. */
  reason: string | null;
  refreshing: boolean;
  /** An action just finished: once the detail is read again, focus goes to Retry if it is still there, else to the heading. */
  returnFocus: boolean;
  onFocused: () => void;
  onRetry: () => void;
  onSkip: () => void;
}) {
  const { enrollment } = path;
  const heading = useRef<HTMLHeadingElement>(null);
  const actions = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!returnFocus || refreshing) return;
    (actions.current?.querySelector<HTMLElement>("button") ?? heading.current)?.focus();
    onFocused();
  }, [returnFocus, refreshing, onFocused]);
  const [cited, setCited] = useState<ReadonlySet<string>>(new Set());
  const [active, setActive] = useState<string | null>(null);
  const shownIds = useMemo(() => new Set(path.decisions.map((record) => record.id)), [path.decisions]);
  useEffect(() => {
    if (active === null) return;
    const row = document.getElementById(decisionRowId(active));
    row?.scrollIntoView({ block: "center" });
    row?.focus();
  }, [active]);
  const failedAt = enrollment.failed_node ? stepTitle(graph, names, enrollment.failed_node) : null;
  const retried = operatorDecisions(path, "retried");
  const skipped = operatorDecisions(path, "skipped");
  const failed = enrollment.state === "failed";
  const describe = (record: TraceTreeRecord) => {
    const node = record.subject_ids["node"];
    return typeof node === "string" ? stepTitle(graph, names, node) : null;
  };
  const recordLabel = (id: string) => {
    const record = path.decisions.find((candidate) => candidate.id === id);
    if (!record) return null;
    const about = describe(record);
    return `${record.action} ${record.outcome}${about ? ` ${about}` : ""}`;
  };

  return (
    <>
      <div className="stack-tight">
        <h3 ref={heading} tabIndex={-1} className="strong">
          Person {shortId(enrollment.person_id)}
        </h3>
        <div className="row">
          <Pill tone={STATE_TONE[enrollment.state] ?? "neutral"}>{failed && failedAt ? `Failed at ${failedAt}` : enrollment.state}</Pill>
          <span className="faint">
            Version {enrollment.version} · entered <time dateTime={enrollment.created_at}>{formatDateTime(enrollment.created_at)}</time>
          </span>
        </div>
        {enrollment.exit_reason ? <span className="muted">Ended: {enrollment.exit_reason}</span> : null}
        {failed && enrollment.error ? <span className="form-error">{enrollment.error}</span> : null}
      </div>
      {failed ? (
        canPublish ? (
          <>
            <div ref={actions} className="row" role="group" aria-label="Operator actions">
              <Button variant="primary" aria-disabled={acting || reason !== null || undefined} onClick={onRetry}>
                {failedAt ? `Retry ${failedAt}` : "Retry"}
              </Button>
              <Button aria-disabled={acting || reason !== null || undefined} onClick={onSkip}>
                Skip it…
              </Button>
            </div>
            {reason ? <p className="faint">{reason}</p> : null}
          </>
        ) : (
          <p className="faint">Retrying or skipping a failed step needs a publisher.</p>
        )
      ) : null}
      <div className="fl-sheet-section">
        <h3>Path</h3>
        {path.steps.length === 0 ? (
          <p className="muted">The enrollment has not run a step yet.</p>
        ) : (
          <ol className="fl-steps" aria-label="Steps this enrollment has run">
            {path.steps.map((step, index) => (
              <li key={`${index}:${step.node_id}`} className="fl-step-row">
                <span className={step.outcome === "failed" ? "fl-step-n fl-step-n-bad" : "fl-step-n"}>{index + 1}</span>
                <div className="stack-tight">
                  <div>
                    <span className="strong">{stepTitle(graph, names, step.node_id)}</span>{" "}
                    <span className="faint mono">{formatDateTime(step.started_at)}</span>
                  </div>
                  <div className="muted">
                    {step.outcome === "failed"
                      ? `Failed after ${step.attempts} ${step.attempts === 1 ? "attempt" : "attempts"}${step.error ? `: ${step.error}` : ""}`
                      : (STEP_OUTCOMES[step.outcome] ?? step.outcome)}
                  </div>
                  {retried.has(step.node_id) ? <div className="faint">Retried by an operator</div> : null}
                  {skipped.has(step.node_id) ? <div className="faint">Skipped by an operator</div> : null}
                </div>
              </li>
            ))}
          </ol>
        )}
        {path.waits.map((wait) => (
          <p key={wait.kind === "event" ? `event:${wait.event}` : "timer"} className="muted">
            {wait.kind === "event"
              ? `Waiting for ${wait.event} until ${formatDateTime(wait.timeout_at)}`
              : `Waiting until ${formatDateTime(wait.wake_at)}`}
          </p>
        ))}
      </div>
      <div className="fl-sheet-section">
        <div className="row">
          <h3>Trace</h3>
          <Link to={traceHref(workspaceId, enrollment.trace_id)}>Open in Traces</Link>
        </div>
        {assistantOn ? (
          <TraceExplanation
            key={enrollment.trace_id}
            workspaceId={workspaceId}
            traceId={enrollment.trace_id}
            recordLabel={recordLabel}
            onGone={onAssistantGone}
            onCited={(ids) => {
              setActive(null);
              setCited(new Set(ids.filter((id) => shownIds.has(id))));
            }}
            onShow={(id) => {
              setActive(null);
              // The same record twice in a row must still scroll, so the state is cleared first.
              setTimeout(() => setActive(id), 0);
            }}
          />
        ) : null}
        <TraceTree
          records={path.decisions}
          label="Decisions on the trace of this enrollment"
          workspaceId={workspaceId}
          describe={describe}
          truncated={path.truncated}
          cited={assistantOn ? { ids: cited, active, rowId: decisionRowId } : undefined}
        />
      </div>
    </>
  );
}
