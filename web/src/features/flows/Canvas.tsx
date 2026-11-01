import "@xyflow/react/dist/style.css";

import {
  Background,
  BackgroundVariant,
  Handle,
  MarkerType,
  Position,
  ReactFlow,
  ReactFlowProvider,
  useReactFlow,
  useUpdateNodeInternals,
  type Connection,
  type Edge,
  type FinalConnectionState,
  type Node,
  type NodeProps,
} from "@xyflow/react";
import { memo, useCallback, useEffect, useMemo, useRef, useState, type CSSProperties, type KeyboardEvent, type ReactNode } from "react";

import { reasonLabel } from "../../ui";
import { wordsFor, triggerWords, type Names } from "./describe";
import { FlowIcon, type IconName } from "./icons";
import { issueText, type PlacedIssues } from "./issues";
import { layout } from "./layout";
import { ASSISTANT_TOGGLE_ID } from "../assistant";
import { COUNT_GAP, COUNT_ROW, MARK_ROW, overlayExtra, TRIGGER_EDGE, type NodeMark, type NodeTally, type Overlay } from "./overlay";
import {
  connectionWire,
  entryOf,
  kindLabel,
  portLabel,
  portsOf,
  TRIGGER_ID,
  type FlowIssue,
  type Graph,
  type Settings,
} from "./model";

interface StepData extends Record<string, unknown> {
  icon: IconName;
  kind: string;
  title: string;
  detail: string;
  mono: boolean;
  ports: { id: string; label: string; name: string }[];
  issueCount: number;
  problem: string | null;
  selected: boolean;
  editable: boolean;
  /** The numbers this step has on a path, in order; empty when it is not on one. */
  stepNumbers: readonly number[];
  dim: boolean;
  counts: NodeTally | null;
  mark: NodeMark | null;
  /** How the assistant's draft treats this step, when the canvas shows the draft. */
  draft: "added" | "changed" | null;
}

type StepNode = Node<StepData, "step">;

function issuesLabel(count: number): string {
  return `${count} issue${count === 1 ? "" : "s"}`;
}

interface CountBadge {
  key: string;
  className: string;
  text: string;
}

/** The badges of a step's counts: the short ones first, one grid cell each, then a full-width one per skip reason. */
function countBadges(counts: NodeTally): CountBadge[] {
  const n = (value: number) => value.toLocaleString("en");
  const short: CountBadge[] = [{ key: "entered", className: "fl-count", text: `${n(counts.entered)} entered` }];
  if (counts.waiting > 0) short.push({ key: "waiting", className: "fl-count fl-count-lite", text: `${n(counts.waiting)} waiting` });
  if (counts.failed > 0) short.push({ key: "failed", className: "fl-count fl-count-bad", text: `${n(counts.failed)} failed` });
  if (counts.sent) short.push({ key: "sent", className: "fl-count", text: `${n(counts.sent)} sent` });
  if (counts.send_failed) short.push({ key: "send_failed", className: "fl-count fl-count-bad", text: `${n(counts.send_failed)} send failed` });
  const skips = (counts.skipped ?? []).map((skip) => ({
    key: `skip-${skip.reason}`,
    className: "fl-count fl-count-lite fl-count-wide",
    text: `${n(skip.count)} skipped: ${reasonLabel(skip.reason)}`,
  }));
  return [...short, ...skips];
}

const Step = memo(function Step({ data }: NodeProps<StepNode>) {
  const many = data.ports.length > 1;
  const classes = [
    "fl-node",
    "nopan",
    data.selected ? "fl-node-selected" : "",
    data.issueCount > 0 ? "fl-node-issue" : "",
    data.dim ? "fl-node-dim" : "",
    data.stepNumbers.length > 0 ? "fl-node-onpath" : "",
    data.mark ? `fl-node-${data.mark.tone}` : "",
    data.draft ? `fl-node-draft fl-node-draft-${data.draft}` : "",
  ]
    .filter(Boolean)
    .join(" ");
  return (
    <div className={classes}>
      {data.icon === "trigger" ? null : (
        <Handle type="target" position={Position.Top} isConnectable={data.editable} aria-label={`Input of ${data.title}`} role="img" />
      )}
      <div className="fl-kind">
        <span className="fl-kind-name">
          <FlowIcon name={data.icon} />
          {data.kind}
        </span>
        {data.draft ? <span className="dock-mark fl-draft-tag" data-change={data.draft}>{data.draft === "added" ? "Added" : "Changed"}</span> : null}
        {data.issueCount > 0 ? (
          <span className="fl-badge">
            <span className="dot dot-error" aria-hidden="true" />
            {issuesLabel(data.issueCount)}
          </span>
        ) : null}
      </div>
      <div className="fl-title">{data.title}</div>
      <div className={data.mono ? "fl-detail mono" : "fl-detail"}>{data.detail}</div>
      {data.stepNumbers.length > 0 ? (
        <span className="fl-step-no" aria-hidden="true">
          {data.stepNumbers.join(", ")}
        </span>
      ) : null}
      {data.mark ? (
        <span className={`fl-mark fl-mark-${data.mark.tone}`} title={data.mark.text} style={{ "--mark-row": `${MARK_ROW}px` } as CSSProperties}>
          {data.mark.text}
        </span>
      ) : null}
      {data.problem ? <div className="fl-node-problem" title={data.problem}>{data.problem}</div> : null}
      {data.counts ? (
        <div className="fl-counts" style={{ "--count-row": `${COUNT_ROW}px`, "--count-gap": `${COUNT_GAP}px` } as CSSProperties}>
          {countBadges(data.counts).map((badge) => (
            <span key={badge.key} className={badge.className} title={badge.text}>
              {badge.text}
            </span>
          ))}
        </div>
      ) : null}
      {many ? (
        <div className="fl-ports" aria-hidden="true">
          {data.ports.map((port) => (
            <span key={port.id} className="fl-port-label">
              {port.label}
            </span>
          ))}
        </div>
      ) : null}
      {data.ports.map((port, index) => (
        <Handle
          key={port.id}
          type="source"
          id={port.id}
          position={Position.Bottom}
          style={{ left: `${((index + 0.5) / data.ports.length) * 100}%` }}
          isConnectable={data.editable && data.icon !== "trigger"}
          aria-label={port.name}
          role="img"
        />
      ))}
    </div>
  );
});

const nodeTypes = { step: Step };

/** The issues of a step and of the connections leaving it. */
function issuesOn(placed: PlacedIssues, graph: Graph, id: string): FlowIssue[] {
  return [...(placed.byNode.get(id) ?? []), ...graph.edges.flatMap((edge) => (edge.from === id ? placed.byEdge.get(edge.id) ?? [] : []))];
}

function motionDuration() {
  return window.matchMedia("(prefers-reduced-motion: reduce)").matches ? 0 : 200;
}

export interface IssueItem {
  key: string;
  /** Where the issue is, in words: the step, the connection or the settings section. */
  where: string;
  issue: FlowIssue;
  /** Whether there is a step or a settings section to open for it; an issue about the whole flow has none. */
  openable: boolean;
}

export interface FocusRequest {
  id: string;
  nonce: number;
  /** Also bring the step into view and mark it for a moment, as when an explanation points at it. */
  reveal?: boolean;
}

interface CanvasProps {
  graph: Graph;
  settings: Settings;
  names: Names;
  placed: PlacedIssues;
  issueItems: readonly IssueItem[];
  /** Whether the check of the saved draft has come back. */
  checked: boolean;
  /** Whether an edit is waiting for the check of its save, so the issues shown may be out of date. */
  checking: boolean;
  editable: boolean;
  selectedId: string | null;
  focusRequest: FocusRequest | null;
  canUndo: boolean;
  onOpen: (id: string) => void;
  onClose: () => void;
  onWire: (from: string, port: string, to: string) => void;
  onUndo: () => void;
  onDeleteRequest: (id: string) => void;
  onPickIssue: (item: IssueItem) => void;
  /** The assistant's tool on the rail; absent when the person has no assistant to open. */
  assistant?: { open: boolean; onToggle: () => void };
  /** Test path, counts or an enrollment drawn over the steps; nothing is drawn when absent. */
  overlay?: Overlay;
  /** Fit the view again whenever this changes, such as when another version is drawn. */
  fitKey?: string | number;
  /** Draw the steps afresh and fit the view whenever this changes, such as when a whole new draft is loaded. */
  layoutKey?: number;
  children?: ReactNode;
}

function Tool({
  id,
  label,
  icon,
  onClick,
  disabled,
  pressed,
}: {
  id?: string;
  label: string;
  icon: IconName;
  onClick: () => void;
  disabled?: boolean;
  pressed?: boolean;
}) {
  return (
    <button
      id={id}
      type="button"
      className="fl-tool"
      aria-label={label}
      aria-pressed={pressed}
      title={label}
      onClick={onClick}
      disabled={disabled}
    >
      <FlowIcon name={icon} size={18} />
    </button>
  );
}

function Rail({
  editable,
  canUndo,
  onUndo,
  assistant,
}: {
  editable: boolean;
  canUndo: boolean;
  onUndo: () => void;
  assistant: CanvasProps["assistant"];
}) {
  const flow = useReactFlow();
  return (
    <div className="fl-rail" role="toolbar" aria-label="Canvas tools">
      <Tool label="Fit to screen" icon="fit" onClick={() => void flow.fitView({ duration: motionDuration(), maxZoom: 1, padding: 0.2 })} />
      <Tool label="Zoom in" icon="zoom-in" onClick={() => void flow.zoomIn({ duration: motionDuration() })} />
      <Tool label="Zoom out" icon="zoom-out" onClick={() => void flow.zoomOut({ duration: motionDuration() })} />
      {editable ? <Tool label="Undo" icon="undo" onClick={onUndo} disabled={!canUndo} /> : null}
      {assistant ? <Tool id={ASSISTANT_TOGGLE_ID} label="Assistant" icon="assistant" onClick={assistant.onToggle} pressed={assistant.open} /> : null}
    </div>
  );
}

function IssuesButton({
  items,
  graph,
  checked,
  checking,
  onPick,
}: {
  items: readonly IssueItem[];
  graph: Graph;
  checked: boolean;
  checking: boolean;
  onPick: (item: IssueItem) => void;
}) {
  const [open, setOpen] = useState(false);
  const wrap = useRef<HTMLDivElement>(null);
  const button = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (!open) return;
    function onPointer(event: MouseEvent) {
      if (wrap.current && event.target instanceof Node && !wrap.current.contains(event.target)) setOpen(false);
    }
    function onKey(event: globalThis.KeyboardEvent) {
      if (event.key !== "Escape") return;
      setOpen(false);
      button.current?.focus();
    }
    document.addEventListener("mousedown", onPointer);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onPointer);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  if (checking) {
    return (
      <span className="fl-clear" role="status">
        Checking…
      </span>
    );
  }
  if (items.length === 0) {
    return checked ? (
      <span className="fl-clear" role="status">
        No issues
      </span>
    ) : null;
  }
  return (
    <div className="fl-issues" ref={wrap}>
      <button
        ref={button}
        type="button"
        className="btn fl-issues-button"
        aria-expanded={open}
        aria-controls="flow-issues-list"
        onClick={() => setOpen((value) => !value)}
      >
        <span className="dot dot-error" aria-hidden="true" />
        {issuesLabel(items.length)}
      </button>
      {open ? (
        <ul id="flow-issues-list" className="fl-issues-list" aria-label="Issues in this flow">
          {items.map((item) => {
            const words = (
              <>
                <span className="fl-issue-where">{item.where}</span>
                <span>{issueText(item.issue, graph)}</span>
              </>
            );
            return (
              <li key={item.key}>
                {item.openable ? (
                  <button
                    type="button"
                    className="fl-issue"
                    onClick={() => {
                      setOpen(false);
                      onPick(item);
                    }}
                  >
                    {words}
                  </button>
                ) : (
                  <div className="fl-issue fl-issue-text">{words}</div>
                )}
              </li>
            );
          })}
        </ul>
      ) : null}
    </div>
  );
}

/** The delivery counts of a send step as words, empty for a step that sends nothing. */
function deliveryWords(counts: NodeTally): string {
  const parts: string[] = [];
  if ((counts.sent ?? 0) > 0) parts.push(`${counts.sent} sent`);
  for (const skip of counts.skipped ?? []) parts.push(`${skip.count} skipped, ${reasonLabel(skip.reason)}`);
  if ((counts.send_failed ?? 0) > 0) parts.push(`${counts.send_failed} send failed`);
  return parts.length === 0 ? "" : `, ${parts.join(", ")}`;
}

/** What the overlay says about one step, as node data and as words for its accessible name. */
function overlayFacts(overlay: Overlay | undefined, id: string) {
  const stepNumbers = overlay?.steps?.get(id) ?? [];
  const counts = overlay?.counts?.get(id) ?? null;
  const mark = overlay?.marks?.get(id) ?? null;
  const dim = overlay?.dimOthers === true && overlay.steps?.has(id) !== true;
  const draft = overlay?.draft?.get(id) ?? null;
  const words = [
    stepNumbers.length > 0 ? `, on the path as step ${stepNumbers.join(" and ")}` : "",
    counts ? `, ${counts.entered} entered, ${counts.waiting} waiting, ${counts.failed} failed${deliveryWords(counts)}` : "",
    mark ? `, ${mark.text}` : "",
    draft ? `, ${draft} in the assistant's draft` : "",
  ].join("");
  return { words, data: { stepNumbers, dim, counts, mark, draft } };
}

const ARROWS = ["ArrowDown", "ArrowUp", "ArrowLeft", "ArrowRight"];

function Surface(props: CanvasProps) {
  const { graph, settings, names, placed, editable, selectedId, focusRequest, overlay } = props;
  const container = useRef<HTMLDivElement>(null);
  const flow = useReactFlow();
  const fitted = useRef(props.fitKey);
  const updateInternals = useUpdateNodeInternals();
  const laidOut = useRef(props.layoutKey);
  /** The step an explanation just pointed at; it is marked until the timer ends. */
  const [flash, setFlash] = useState<string | null>(null);
  const flashTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  useEffect(() => () => clearTimeout(flashTimer.current), []);
  const counts = overlay?.counts;
  const marks = overlay?.marks;
  const plan = useMemo(() => {
    const extra = new Map<string, number>();
    for (const [id, tally] of counts ?? []) extra.set(id, overlayExtra(tally, marks?.has(id) === true));
    for (const id of [TRIGGER_ID, ...graph.nodes.map((node) => node.id)]) {
      if (issuesOn(placed, graph, id).length > 0) extra.set(id, (extra.get(id) ?? 0) + 60);
    }
    return layout(graph, extra);
  }, [graph, counts, marks, placed]);

  const nodes = useMemo(() => {
    const byId = new Map(graph.nodes.map((node) => [node.id, node]));
    const total = plan.order.filter((id) => id !== TRIGGER_ID).length;
    const list: StepNode[] = [];
    let step = 0;
    for (const id of plan.order) {
      if (id !== TRIGGER_ID) step += 1;
      const spot = plan.placed.get(id);
      if (!spot) continue;
      const problems = issuesOn(placed, graph, id);
      const count = problems.length;
      const problem = problems[0] ? issueText(problems[0], graph) : null;
      const extra = overlayFacts(overlay, id);
      const trailing = (count > 0 ? `, ${issuesLabel(count)}` : "") + extra.words;
      const base = {
        id,
        type: "step",
        position: { x: spot.x, y: spot.y },
        width: spot.width,
        height: spot.height,
        draggable: false,
        className: flash === id ? "nopan fl-node-flash" : "nopan",
        deletable: false,
        selected: selectedId === id,
      } as const;
      if (id === TRIGGER_ID) {
        const words = triggerWords(settings, names);
        list.push({
          ...base,
          ariaLabel: `Trigger: ${words.title}${trailing}`,
          data: {
            icon: "trigger",
            kind: "Trigger",
            title: words.title,
            detail: words.detail,
            mono: false,
            ports: [{ id: "next", label: "Next", name: "Output of the trigger" }],
            issueCount: count,
            problem,
            selected: base.selected,
            editable: false,
            ...extra.data,
          },
        });
        continue;
      }
      const node = byId.get(id);
      if (!node) continue;
      const words = wordsFor(node, names);
      list.push({
        ...base,
        ariaLabel: `${kindLabel(node.type)}: ${words.title}, step ${step} of ${total}${trailing}`,
        data: {
          icon: node.type,
          kind: kindLabel(node.type),
          title: words.title,
          detail: words.detail,
          mono: node.type === "action.webhook",
          ports: portsOf(node).map((port) => ({
            id: port,
            label: portLabel(node, port),
            name: `${portLabel(node, port)} output of ${words.title}`,
          })),
          issueCount: count,
          problem,
          selected: base.selected,
          editable,
          ...extra.data,
        },
      });
    }
    return list;
  }, [plan, graph.nodes, settings, names, placed, selectedId, editable, overlay, flash]);

  const edges = useMemo(() => {
    const known = new Set(plan.order);
    const list: Edge[] = [];
    const entry = entryOf(graph);
    if (entry) {
      list.push({
        id: TRIGGER_EDGE,
        source: TRIGGER_ID,
        sourceHandle: "next",
        target: entry.id,
        type: "smoothstep",
        markerEnd: { type: MarkerType.ArrowClosed },
        selectable: false,
        focusable: false,
        deletable: false,
        className: overlay?.pathEdges?.has(TRIGGER_EDGE) ? "fl-edge-path" : undefined,
      });
    }
    for (const edge of graph.edges) {
      if (!known.has(edge.from) || !known.has(edge.to)) continue;
      const problems = placed.byEdge.get(edge.id);
      list.push({
        id: edge.id,
        source: edge.from,
        sourceHandle: edge.port,
        target: edge.to,
        type: "smoothstep",
        markerEnd: { type: MarkerType.ArrowClosed },
        selectable: false,
        focusable: false,
        deletable: false,
        className: problems ? "fl-edge-error" : overlay?.pathEdges?.has(edge.id) ? "fl-edge-path" : undefined,
        label: problems ? "Needs attention" : undefined,
        ariaLabel: problems ? problems.map((issue) => issueText(issue)).join(" ") : undefined,
      });
    }
    return list;
  }, [plan, graph, placed.byEdge, overlay]);

  useEffect(() => {
    if (fitted.current === props.fitKey) return;
    fitted.current = props.fitKey;
    const timer = setTimeout(() => void flow.fitView({ maxZoom: 1, padding: 0.2 }), 0);
    return () => clearTimeout(timer);
  }, [props.fitKey, flow]);

  useEffect(() => {
    if (laidOut.current === props.layoutKey) return;
    laidOut.current = props.layoutKey;
    // The steps are new, so their handles are measured again before the wires are drawn, then the view is fitted.
    const timer = setTimeout(() => {
      updateInternals(plan.order);
      void flow.fitView({ maxZoom: 1, padding: 0.2 });
    }, 0);
    return () => clearTimeout(timer);
  }, [props.layoutKey, plan, flow, updateInternals]);

  /** Brings a step into view and marks it for a moment: a mouse click leaves no focus ring to show where it is. */
  const reveal = useCallback(
    (id: string) => {
      void flow.fitView({ nodes: [{ id }], maxZoom: 1, padding: 0.5, duration: motionDuration() });
      clearTimeout(flashTimer.current);
      setFlash(id);
      flashTimer.current = setTimeout(() => setFlash(null), 2500);
    },
    [flow],
  );

  function focusNode(id: string) {
    const target = Array.from(container.current?.querySelectorAll<HTMLElement>(".react-flow__node") ?? []).find(
      (element) => element.dataset["id"] === id,
    );
    target?.focus();
  }

  useEffect(() => {
    if (focusRequest) {
      focusNode(focusRequest.id);
      if (focusRequest.reveal) {
        const id = focusRequest.id;
        const timer = setTimeout(() => reveal(id), 0);
        return () => clearTimeout(timer);
      }
    }
    // Only a new request moves focus; the same request seen again must not.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [focusRequest?.nonce]);

  function neighbour(id: string, key: string): string | null {
    if (key === "ArrowDown") {
      if (id === TRIGGER_ID) return entryOf(graph)?.id ?? null;
      const node = graph.nodes.find((candidate) => candidate.id === id);
      if (!node) return null;
      for (const port of portsOf(node)) {
        const edge = graph.edges.find((candidate) => candidate.from === id && candidate.port === port);
        if (edge && plan.placed.has(edge.to)) return edge.to;
      }
      return null;
    }
    if (key === "ArrowUp") {
      const edge = graph.edges.find((candidate) => candidate.to === id && plan.placed.has(candidate.from));
      if (edge) return edge.from;
      return id !== TRIGGER_ID && entryOf(graph)?.id === id ? TRIGGER_ID : null;
    }
    const row = plan.rows.find((ids) => ids.includes(id));
    if (!row) return null;
    const at = row.indexOf(id) + (key === "ArrowRight" ? 1 : -1);
    return row[at] ?? null;
  }

  function onKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    const target = event.target;
    if (!(target instanceof HTMLElement)) return;
    if (event.key === "Escape" && selectedId !== null) {
      event.stopPropagation();
      props.onClose();
      return;
    }
    const wrapper = target.closest<HTMLElement>(".react-flow__node");
    if (!wrapper || wrapper !== target) return;
    const id = wrapper.dataset["id"];
    if (id === undefined) return;
    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      props.onOpen(id);
    } else if (ARROWS.includes(event.key)) {
      event.preventDefault();
      const next = neighbour(id, event.key);
      if (next !== null) focusNode(next);
    } else if ((event.key === "Delete" || event.key === "Backspace") && editable && id !== TRIGGER_ID) {
      event.preventDefault();
      props.onDeleteRequest(id);
    }
  }

  function onConnect(connection: Connection) {
    const wire = connectionWire(connection, editable);
    if (wire) props.onWire(wire.from, wire.port, wire.to);
  }

  /** A drag from an output that lets go over a step, but not over its input dot, wires to that step too. */
  function onConnectEnd(event: MouseEvent | TouchEvent, state: FinalConnectionState) {
    if (state.isValid || !state.fromHandle || state.fromHandle.type !== "source" || !state.fromHandle.id) return;
    const point = "changedTouches" in event ? event.changedTouches[0] : event;
    if (!point) return;
    const over = document.elementFromPoint(point.clientX, point.clientY)?.closest<HTMLElement>(".react-flow__node");
    const target = over?.dataset["id"];
    if (target === undefined) return;
    const wire = connectionWire({ source: state.fromNode.id, sourceHandle: state.fromHandle.id, target }, editable);
    if (wire) props.onWire(wire.from, wire.port, wire.to);
  }

  return (
    <>
      <div className="fl-tools">
        <Rail editable={editable} canUndo={props.canUndo} onUndo={props.onUndo} assistant={props.assistant} />
        <IssuesButton
          items={props.issueItems}
          graph={graph}
          checked={props.checked}
          checking={props.checking}
          onPick={props.onPickIssue}
        />
      </div>
      <div className="fl-flow" ref={container} onKeyDown={onKeyDown}>
        <ReactFlow
          nodes={nodes}
          edges={edges}
          nodeTypes={nodeTypes}
          fitView
          fitViewOptions={{ maxZoom: 1, padding: 0.2 }}
          minZoom={0.2}
          nodesDraggable={false}
          nodesConnectable={editable}
          elementsSelectable
          disableKeyboardA11y
          deleteKeyCode={null}
          selectionKeyCode={null}
          multiSelectionKeyCode={null}
          zoomOnDoubleClick={false}
          onNodeClick={(_, node) => props.onOpen(node.id)}
          onConnect={onConnect}
          onConnectEnd={onConnectEnd}
          attributionPosition="bottom-left"
        >
          <Background variant={BackgroundVariant.Dots} gap={20} size={1} />
        </ReactFlow>
      </div>
      {props.children}
    </>
  );
}

/** The full-width canvas of a flow: the steps, the wires between them, the tool rail and the issues. */
export function Canvas(props: CanvasProps) {
  return (
    <section className="fl-canvas" aria-label="Flow canvas">
      <ReactFlowProvider>
        <Surface {...props} />
      </ReactFlowProvider>
    </section>
  );
}
