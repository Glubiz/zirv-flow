import { useEffect, useRef, useState, type RefObject } from "react";

import { client, unwrap, type components } from "../../api";
import { Button, EmptyState, InlineError, LoadingState, Sheet } from "../../ui";
import { AssistantPanel, failureText, type Failure } from "../assistant";
import { wordsFor, type Names } from "./describe";
import { kindLabel, TRIGGER_ID, type Graph } from "./model";
import { SimulationSummary, stepTitle } from "./SimulationSummary";

type Explained = components["schemas"]["AiFlowExplainBody"];

export interface FlowDrafting {
  /** Asked before the accept request; false keeps the person's edits and accepts nothing. */
  confirmAccept: () => boolean | Promise<boolean>;
  /** Loads the accepted draft (graph and settings) into the editor. */
  onAccept: (content: unknown) => void;
  /** The accept request failed after the editor was made ready for it. */
  onAcceptFailed: () => void;
}

interface Props {
  workspaceId: string;
  flowId: string;
  /** The published version to explain; null for the flow's draft. */
  version: number | null;
  /** The graph on the canvas, to name the steps the explanation cites. */
  graph: Graph;
  names: Names;
  /** Only the draft can be drafted over; a version is read-only. */
  drafting?: FlowDrafting;
  /** Saves what is waiting so the server explains what is on the canvas; false when it could not. */
  prepare?: () => Promise<boolean>;
  /** Shows a cited step on the canvas. */
  onReveal: (id: string) => void;
  /** The steps the explanation cites that the canvas has, to be marked there; empty when none or when the sheet closes. */
  onCited: (ids: readonly string[]) => void;
  onClose: () => void;
}

/** The graph of a drafted flow document, or null when it does not look like one. */
function graphOf(content: unknown): Graph | null {
  if (typeof content !== "object" || content === null) return null;
  const graph = (content as { graph?: unknown }).graph;
  if (typeof graph !== "object" || graph === null) return null;
  return Array.isArray((graph as { nodes?: unknown }).nodes) ? (graph as Graph) : null;
}

/** The assistant's right-hand sheet of the flow editor: explain the flow and, for the draft, draft changes to it. */
export function FlowAssistantSheet(props: Props) {
  const { workspaceId, flowId, version, graph, names, drafting } = props;
  return (
    <Sheet
      title="Assistant"
      eyebrow={version === null ? "The draft" : `Version ${version}`}
      closeLabel="Close the assistant"
      onClose={props.onClose}
    >
      <div className="fl-sheet-section">
        <FlowExplanation
          key={version ?? "draft"}
          workspaceId={workspaceId}
          flowId={flowId}
          version={version}
          graph={graph}
          names={names}
          prepare={props.prepare}
          onReveal={props.onReveal}
          onCited={props.onCited}
        />
      </div>
      {drafting ? (
        <div className="fl-sheet-section">
          <AssistantPanel
            workspaceId={workspaceId}
            kind="flow"
            flowId={flowId}
            onAccept={drafting.onAccept}
            confirmAccept={drafting.confirmAccept}
            onAcceptFailed={drafting.onAcceptFailed}
            renderPreview={(content, draft) =>
              draft.simulation ? <SimulationSummary simulation={draft.simulation} graph={graphOf(content)} names={names} /> : null
            }
          />
        </div>
      ) : null}
    </Sheet>
  );
}

type Run =
  | { status: "idle" }
  | { status: "running" }
  | { status: "failed"; failure: Failure }
  | { status: "done"; explained: Explained };

function FlowExplanation({
  workspaceId,
  flowId,
  version,
  graph,
  names,
  prepare,
  onReveal,
  onCited,
}: Pick<Props, "workspaceId" | "flowId" | "version" | "graph" | "names" | "prepare" | "onReveal" | "onCited">) {
  const [run, setRun] = useState<Run>({ status: "idle" });
  const ticket = useRef(0);
  const heading = useRef<HTMLHeadingElement>(null);
  const noSteps = graph.nodes.length === 0;
  const subject = version === null ? "this flow" : `version ${version}`;

  useEffect(
    () => () => {
      ticket.current += 1;
      onCited([]);
    },
    // Only the moment the explanation goes matters: it clears what it marked.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [],
  );

  const shown = run.status === "done" ? run.explained : null;
  useEffect(() => {
    if (shown !== null) heading.current?.focus();
  }, [shown]);

  async function explain() {
    if (run.status === "running") return;
    ticket.current += 1;
    const mine = ticket.current;
    setRun({ status: "running" });
    onCited([]);
    try {
      if (prepare && !(await prepare())) {
        if (mine === ticket.current) {
          setRun({
            status: "failed",
            failure: {
              title: "Your latest changes could not be saved",
              detail: "The assistant explains the saved draft, so it would describe an older one. Fix the save, then ask again.",
              traceId: null,
            },
          });
        }
        return;
      }
      const explained = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/flows/{id}/ai/explain", {
          params: { path: { ws: workspaceId, id: flowId } },
          ...(version === null ? {} : { body: { version } }),
        }),
      );
      if (mine !== ticket.current) return;
      setRun({ status: "done", explained });
      onCited(explained.nodes.map((entry) => entry.id).filter((id) => graph.nodes.some((node) => node.id === id)));
    } catch (error) {
      if (mine === ticket.current) setRun({ status: "failed", failure: failureText(error) });
    }
  }

  return (
    <section className="stack-tight" aria-label="Explain this flow">
      <h3>Explain this flow</h3>
      {noSteps && version === null ? (
        <EmptyState title="Nothing to explain yet">Add a step to the flow first.</EmptyState>
      ) : (
        <div className="row">
          <Button variant={run.status === "done" ? "quiet" : "primary"} onClick={() => void explain()} disabled={run.status === "running"}>
            {run.status === "done" || run.status === "failed"
              ? "Explain again"
              : version === null
                ? "Explain the draft"
                : `Explain version ${version}`}
          </Button>
        </div>
      )}
      <p className="visually-hidden" role="status">
        {run.status === "running" ? "Asking the assistant" : run.status === "done" ? "Explanation ready" : ""}
      </p>
      {run.status === "running" ? <LoadingState label={`Asking the assistant to explain ${subject}. This can take a minute.`} /> : null}
      {run.status === "failed" ? <InlineError {...run.failure} /> : null}
      {run.status === "done" ? (
        <Explanation explained={run.explained} graph={graph} names={names} heading={heading} onReveal={onReveal} />
      ) : null}
    </section>
  );
}

function Explanation({
  explained,
  graph,
  names,
  heading,
  onReveal,
}: {
  explained: Explained;
  graph: Graph;
  names: Names;
  heading: RefObject<HTMLHeadingElement | null>;
  onReveal: (id: string) => void;
}) {
  return (
    <div className="stack-tight">
      <h4 ref={heading} tabIndex={-1}>
        {explained.source === "version" && explained.version != null
          ? `Version ${explained.version}, as the assistant reads it`
          : "The draft, as the assistant reads it"}
      </h4>
      <p className="faint">Written by the assistant. Check it against the steps: the names below are the canvas&rsquo;s own.</p>
      <p>{explained.summary}</p>
      {explained.nodes.length === 0 ? (
        <p className="muted">The assistant cited no steps.</p>
      ) : (
        <ol className="list-plain fl-cites" aria-label="Step by step">
          {explained.nodes.map((entry) => {
            const node = graph.nodes.find((candidate) => candidate.id === entry.id);
            const label = node ? `${kindLabel(node.type)}: ${wordsFor(node, names).title}` : null;
            return (
              <li key={entry.id} className="fl-cite">
                {node && entry.id !== TRIGGER_ID ? (
                  <Button onClick={() => onReveal(entry.id)}>
                    Show on the canvas: {label} <span className="faint mono">{entry.id}</span>
                  </Button>
                ) : (
                  <span className="muted">
                    {stepTitle(graph, names, entry.id)} <span className="faint mono">{entry.id}</span> (not on the canvas now)
                  </span>
                )}
                <span>{entry.text}</span>
              </li>
            );
          })}
        </ol>
      )}
    </div>
  );
}
