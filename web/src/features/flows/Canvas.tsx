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
  type Connection,
  type Edge,
  type Node,
  type NodeProps,
} from "@xyflow/react";
import { memo, useEffect, useMemo, useRef, useState, type KeyboardEvent, type ReactNode } from "react";

import { MenuButton } from "../../ui";
import { wordsFor, triggerWords, type Names } from "./describe";
import { FlowIcon, type IconName } from "./icons";
import { issueText, type PlacedIssues } from "./issues";
import { layout } from "./layout";
import {
  entryOf,
  KINDS,
  kindLabel,
  portLabel,
  portsOf,
  TRIGGER_ID,
  type FlowIssue,
  type Graph,
  type NodeType,
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
  selected: boolean;
  editable: boolean;
}

type StepNode = Node<StepData, "step">;

function issuesLabel(count: number): string {
  return `${count} issue${count === 1 ? "" : "s"}`;
}

const Step = memo(function Step({ data }: NodeProps<StepNode>) {
  const many = data.ports.length > 1;
  const classes = ["fl-node", "nopan", data.selected ? "fl-node-selected" : "", data.issueCount > 0 ? "fl-node-issue" : ""]
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
        {data.issueCount > 0 ? (
          <span className="fl-badge">
            <span className="dot dot-error" aria-hidden="true" />
            {issuesLabel(data.issueCount)}
          </span>
        ) : null}
      </div>
      <div className="fl-title">{data.title}</div>
      <div className={data.mono ? "fl-detail mono" : "fl-detail"}>{data.detail}</div>
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

export interface IssueItem {
  key: string;
  /** Where the issue is, in words: the step, the connection or the settings section. */
  where: string;
  issue: FlowIssue;
}

export interface FocusRequest {
  id: string;
  nonce: number;
}

interface CanvasProps {
  graph: Graph;
  settings: Settings;
  names: Names;
  placed: PlacedIssues;
  issueItems: readonly IssueItem[];
  /** Whether the check of the saved draft has come back. */
  checked: boolean;
  editable: boolean;
  selectedId: string | null;
  focusRequest: FocusRequest | null;
  canUndo: boolean;
  onOpen: (id: string) => void;
  onClose: () => void;
  onWire: (from: string, port: string, to: string) => void;
  onAdd: (type: NodeType) => void;
  onUndo: () => void;
  onDeleteRequest: (id: string) => void;
  onPickIssue: (item: IssueItem) => void;
  children?: ReactNode;
}

function Tool({ label, icon, onClick, disabled }: { label: string; icon: IconName; onClick: () => void; disabled?: boolean }) {
  return (
    <button type="button" className="fl-tool" aria-label={label} title={label} onClick={onClick} disabled={disabled}>
      <FlowIcon name={icon} size={18} />
    </button>
  );
}

function Rail({
  editable,
  canUndo,
  onAdd,
  onUndo,
}: {
  editable: boolean;
  canUndo: boolean;
  onAdd: (type: NodeType) => void;
  onUndo: () => void;
}) {
  const flow = useReactFlow();
  return (
    <div className="fl-rail" role="toolbar" aria-label="Canvas tools">
      {editable ? (
        <MenuButton
          label="Add a step"
          buttonClassName="fl-tool"
          buttonContent={<FlowIcon name="plus" size={18} />}
          heading="Add a step"
          entries={KINDS.map((kind) => ({ key: kind.type, label: kind.label, onSelect: () => onAdd(kind.type) }))}
        />
      ) : null}
      <Tool label="Fit to screen" icon="fit" onClick={() => void flow.fitView({ duration: 200, maxZoom: 1, padding: 0.2 })} />
      <Tool label="Zoom in" icon="zoom-in" onClick={() => void flow.zoomIn({ duration: 200 })} />
      <Tool label="Zoom out" icon="zoom-out" onClick={() => void flow.zoomOut({ duration: 200 })} />
      {editable ? <Tool label="Undo" icon="undo" onClick={onUndo} disabled={!canUndo} /> : null}
    </div>
  );
}

function IssuesButton({
  items,
  checked,
  onPick,
}: {
  items: readonly IssueItem[];
  checked: boolean;
  onPick: (item: IssueItem) => void;
}) {
  const [open, setOpen] = useState(false);
  if (items.length === 0) {
    return checked ? (
      <span className="fl-clear" role="status">
        No issues
      </span>
    ) : null;
  }
  return (
    <div className="fl-issues">
      <button type="button" className="btn fl-issues-button" aria-expanded={open} aria-controls="flow-issues-list" onClick={() => setOpen((value) => !value)}>
        <span className="dot dot-error" aria-hidden="true" />
        {issuesLabel(items.length)}
      </button>
      {open ? (
        <ul id="flow-issues-list" className="fl-issues-list" aria-label="Issues in this flow">
          {items.map((item) => (
            <li key={item.key}>
              <button
                type="button"
                className="fl-issue"
                onClick={() => {
                  setOpen(false);
                  onPick(item);
                }}
              >
                <span className="fl-issue-where">{item.where}</span>
                <span>{issueText(item.issue)}</span>
              </button>
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}

const ARROWS = ["ArrowDown", "ArrowUp", "ArrowLeft", "ArrowRight"];

function Surface(props: CanvasProps) {
  const { graph, settings, names, placed, editable, selectedId, focusRequest } = props;
  const container = useRef<HTMLDivElement>(null);
  const plan = useMemo(() => layout(graph), [graph]);

  const nodes = useMemo(() => {
    const steps = plan.order.filter((id) => id !== TRIGGER_ID);
    const total = steps.length;
    const list: StepNode[] = [];
    for (const id of plan.order) {
      const spot = plan.placed.get(id);
      if (!spot) continue;
      const count = placed.byNode.get(id)?.length ?? 0;
      const trailing = count > 0 ? `, ${issuesLabel(count)}` : "";
      if (id === TRIGGER_ID) {
        const words = triggerWords(settings, names);
        list.push({
          id,
          type: "step",
          position: { x: spot.x, y: spot.y },
          width: spot.width,
          height: spot.height,
          draggable: false,
          className: "nopan",
          deletable: false,
          selected: selectedId === id,
          ariaLabel: `Trigger: ${words.title}${trailing}`,
          data: {
            icon: "trigger",
            kind: "Trigger",
            title: words.title,
            detail: words.detail,
            mono: false,
            ports: [{ id: "next", label: "Next", name: `Output of the trigger` }],
            issueCount: count,
            selected: selectedId === id,
            editable: false,
          },
        });
        continue;
      }
      const node = graph.nodes.find((candidate) => candidate.id === id);
      if (!node) continue;
      const words = wordsFor(node, names);
      const step = steps.indexOf(id) + 1;
      list.push({
        id,
        type: "step",
        position: { x: spot.x, y: spot.y },
        width: spot.width,
        height: spot.height,
        draggable: false,
        className: "nopan",
        deletable: false,
        selected: selectedId === id,
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
          selected: selectedId === id,
          editable,
        },
      });
    }
    return list;
  }, [plan, graph.nodes, settings, names, placed, selectedId, editable]);

  const edges = useMemo(() => {
    const known = new Set(plan.order);
    const list: Edge[] = [];
    const entry = entryOf(graph);
    if (entry) {
      list.push({
        id: "__trigger-edge",
        source: TRIGGER_ID,
        sourceHandle: "next",
        target: entry.id,
        type: "smoothstep",
        markerEnd: { type: MarkerType.ArrowClosed },
        selectable: false,
        focusable: false,
        deletable: false,
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
        className: problems ? "fl-edge-error" : undefined,
        label: problems ? "Needs attention" : undefined,
        ariaLabel: problems ? problems.map((issue) => issueText(issue)).join(" ") : undefined,
      });
    }
    return list;
  }, [plan, graph, placed.byEdge]);

  function focusNode(id: string) {
    const target = Array.from(container.current?.querySelectorAll<HTMLElement>(".react-flow__node") ?? []).find(
      (element) => element.dataset["id"] === id,
    );
    target?.focus();
  }

  useEffect(() => {
    if (focusRequest) focusNode(focusRequest.id);
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
    if (!editable || !connection.sourceHandle) return;
    if (connection.source === TRIGGER_ID || connection.source === connection.target) return;
    props.onWire(connection.source, connection.sourceHandle, connection.target);
  }

  return (
    <>
      <div className="fl-tools">
        <Rail editable={editable} canUndo={props.canUndo} onAdd={props.onAdd} onUndo={props.onUndo} />
        <IssuesButton items={props.issueItems} checked={props.checked} onPick={props.onPickIssue} />
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
