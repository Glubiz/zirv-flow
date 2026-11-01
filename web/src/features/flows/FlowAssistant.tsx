import { useEffect, useRef, useState, type RefObject } from "react";

import { client, unwrap, type components } from "../../api";
import { Button, EmptyState, InlineError, LoadingState } from "../../ui";
import { AssistantDock, AssistantPanel, assistantGone, failureText, type Draft, type Failure } from "../assistant";
import type { Names } from "./describe";
import { FlowChanges } from "./FlowChanges";
import { TRIGGER_ID, type Graph } from "./model";
import { SimulationSummary, stepTitle } from "./SimulationSummary";

type Explained = components["schemas"]["AiFlowExplainBody"];

export interface FlowDrafting {
  /** Asked before the accept request; false keeps the person's edits and accepts nothing. */
  confirmAccept: () => boolean | Promise<boolean>;
  /** Loads the accepted draft (graph and settings) into the editor. */
  onAccept: (content: unknown) => void;
  /** The accept request failed after the editor was made ready for it. */
  onAcceptFailed: () => unknown;
}

interface Props {
  workspaceId: string;
  flowId: string;
  /** The published version to explain; null for the flow's draft. */
  version: number | null;
  /** The graph on the canvas, to name the steps the explanation cites. */
  graph: Graph;
  /**
   * What an explanation is of: it is stale as soon as any of these is no longer the same object, such
   * as the graph or the settings after an edit or an accepted draft.
   */
  revision: readonly unknown[];
  /** A request was refused because the assistant is off or no longer the person's to use. */
  onGone: () => void;
  /** An accepted draft is loading: nothing is asked of the server about the draft until it lands. */
  busy?: boolean;
  names: Names;
  /** Only the draft can be drafted over; a version is read-only. */
  drafting?: FlowDrafting;
  /** Saves what is waiting so the server explains what is on the canvas; false when it could not. */
  prepare?: () => Promise<boolean>;
  /** The valid draft the assistant is showing, or null: the canvas marks what it adds and changes. */
  onDraft?: (draft: Draft | null) => void;
  /** Keeps the dock mounted but out of sight, such as while a step's inspector takes its place. */
  hidden?: boolean;
  /** Shows a cited step on the canvas. */
  onReveal: (id: string) => void;
  /** The steps the explanation cites that the canvas has, to be marked there; empty when none or when the sheet closes. */
  onCited: (ids: readonly string[]) => void;
  onClose: () => void;
}

/** The graph of a drafted flow document, or null when it does not look like one. */
export function graphOf(content: unknown): Graph | null {
  if (typeof content !== "object" || content === null) return null;
  const graph = (content as { graph?: unknown }).graph;
  if (typeof graph !== "object" || graph === null) return null;
  return Array.isArray((graph as { nodes?: unknown }).nodes) ? (graph as Graph) : null;
}

/** The assistant dock of the flow editor: explain the flow and, for the draft, draft changes to it. */
export function FlowAssistantDock(props: Props) {
  const { workspaceId, flowId, version, graph, names, drafting, onGone, busy } = props;
  return (
    <AssistantDock
      thing="flow"
      hidden={props.hidden}
      onClose={props.onClose}
      draft={
        drafting ? (
          <AssistantPanel
            workspaceId={workspaceId}
            kind="flow"
            flowId={flowId}
            onAccept={drafting.onAccept}
            confirmAccept={drafting.confirmAccept}
            onAcceptFailed={drafting.onAcceptFailed}
            onDraftShown={props.onDraft}
            onFailure={(error) => {
              if (assistantGone(error)) onGone();
            }}
            renderChanges={(changes, draft) => (
              <FlowChanges changes={changes} before={graph} after={graphOf(draft.content)} names={names} />
            )}
            renderPreview={(content, draft) =>
              draft.simulation ? <SimulationSummary simulation={draft.simulation} graph={graphOf(content)} names={names} /> : null
            }
          />
        ) : undefined
      }
      explain={
        <FlowExplanation
          key={version ?? "draft"}
          workspaceId={workspaceId}
          flowId={flowId}
          version={version}
          graph={graph}
          revision={props.revision}
          names={names}
          onGone={onGone}
          busy={busy}
          prepare={props.prepare}
          onReveal={props.onReveal}
          onCited={props.onCited}
        />
      }
    />
  );
}

type Run =
  | { status: "idle" }
  | { status: "running" }
  | { status: "failed"; failure: Failure }
  | { status: "done"; explained: Explained; revision: readonly unknown[] };

function sameRevision(left: readonly unknown[], right: readonly unknown[]): boolean {
  return left.length === right.length && left.every((item, index) => Object.is(item, right[index]));
}

const CHANGED_WHILE_ASKING: Failure = {
  title: "The draft changed while the assistant was answering",
  detail: "Its answer was about the draft as it was, so it is not shown. Ask again.",
  traceId: null,
};

/** Whether the canvas draws this step: the trigger card is not in the graph but is always drawn. */
function onCanvas(graph: Graph, id: string): boolean {
  return id === TRIGGER_ID || graph.nodes.some((node) => node.id === id);
}

function FlowExplanation({
  workspaceId,
  flowId,
  version,
  graph,
  revision,
  names,
  onGone,
  busy = false,
  prepare,
  onReveal,
  onCited,
}: Pick<
  Props,
  "workspaceId" | "flowId" | "version" | "graph" | "revision" | "names" | "onGone" | "busy" | "prepare" | "onReveal" | "onCited"
>) {
  const [run, setRun] = useState<Run>({ status: "idle" });
  const ticket = useRef(0);
  const current = useRef(revision);
  useEffect(() => {
    current.current = revision;
  });
  const stale = run.status === "done" && !sameRevision(run.revision, revision);
  useEffect(() => {
    if (stale) onCited([]);
    // The marks are cleared once, when the shown result goes stale.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [stale]);
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
    const asked = revision;
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
      if (!sameRevision(asked, current.current)) {
        setRun({ status: "failed", failure: CHANGED_WHILE_ASKING });
        return;
      }
      setRun({ status: "done", explained, revision: asked });
      onCited(explained.nodes.map((entry) => entry.id).filter((id) => onCanvas(graph, id)));
    } catch (error) {
      if (mine !== ticket.current) return;
      if (assistantGone(error)) onGone();
      setRun({ status: "failed", failure: failureText(error) });
    }
  }

  return (
    <section className="dock-body" aria-label="Explain this flow">
      {noSteps && version === null ? (
        <EmptyState title="Nothing to explain yet">Add a step to the flow first.</EmptyState>
      ) : (
        <div className="row">
          <Button variant={run.status === "done" ? "quiet" : "primary"} onClick={() => void explain()} disabled={run.status === "running" || busy}>
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
        <Explanation explained={run.explained} stale={stale} graph={graph} names={names} heading={heading} onReveal={onReveal} />
      ) : null}
    </section>
  );
}

function Explanation({
  explained,
  stale,
  graph,
  names,
  heading,
  onReveal,
}: {
  explained: Explained;
  stale: boolean;
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
      {stale ? (
        <p className="form-error" role="alert">
          The draft changed since this explanation. It describes the draft as it was; use Explain again.
        </p>
      ) : null}
      <p className="faint">Written by the assistant. Check it against the steps: the names below are the canvas&rsquo;s own.</p>
      <p>{explained.summary}</p>
      {explained.nodes.length === 0 ? (
        <p className="muted">The assistant cited no steps.</p>
      ) : (
        <ol className="list-plain fl-cites" aria-label="Step by step">
          {explained.nodes.map((entry) => {
                        return (
              <li key={entry.id} className="fl-cite">
                {onCanvas(graph, entry.id) ? (
                  <button
                    type="button"
                    className="fl-cite-link"
                    aria-label={`Show on the canvas: ${stepTitle(graph, names, entry.id)} ${entry.id}`}
                    onClick={() => onReveal(entry.id)}
                  >
                    <span>{stepTitle(graph, names, entry.id)}</span>
                    <span className="fl-id mono">{entry.id}</span>
                  </button>
                ) : (
                  <span className="muted">
                    {stepTitle(graph, names, entry.id)} <span className="faint mono fl-id">{entry.id}</span> (not on the canvas now)
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
