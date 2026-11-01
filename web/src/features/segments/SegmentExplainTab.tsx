import { useEffect, useRef, useState } from "react";

import { client, unwrap, type components } from "../../api";
import { Button, InlineError, LoadingState } from "../../ui";
import { assistantGone, failureText, type Failure } from "../assistant";
import { fromDefinition, getAt, place, type Definition } from "./ast";
import { describeNode, type Names } from "./describe";

type Explained = components["schemas"]["AiExplainBody"];

interface Props {
  workspaceId: string;
  segmentId: string;
  /** The saved version's definition: the explanation is about it, and its conditions are described in words beside the assistant's text. */
  definition: Definition;
  /** The saved version's number; an explanation of an older one is marked as stale. */
  version: number;
  names: Names;
  /** The condition being explained, by JSON pointer; the builder marks it. */
  selected: string | null;
  onSelect: (pointer: string | null) => void;
  /** A request was refused because the assistant is off or no longer the person's to use. */
  onGone: () => void;
}

type Run = { status: "idle" } | { status: "running" } | { status: "failed"; failure: Failure } | { status: "done"; explained: Explained };

/**
 * The "Explain this segment" tab of the assistant dock: a summary and, for each condition, what it
 * says in words (the builder's own description) with the assistant's sentence. Choosing a condition
 * marks it in the builder. The sentences are plain text and are never rendered as markup.
 */
export function SegmentExplainTab({ workspaceId, segmentId, definition, version, names, selected, onSelect, onGone }: Props) {
  const [run, setRun] = useState<Run>({ status: "idle" });
  const ticket = useRef(0);
  const heading = useRef<HTMLHeadingElement>(null);
  const shown = run.status === "done" ? run.explained : null;

  useEffect(
    () => () => {
      ticket.current += 1;
      onSelect(null);
    },
    // Only the moment the tab goes matters: it clears what it marked.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [],
  );
  useEffect(() => {
    if (shown !== null) heading.current?.focus();
  }, [shown]);

  async function explain() {
    if (run.status === "running") return;
    ticket.current += 1;
    const mine = ticket.current;
    setRun({ status: "running" });
    onSelect(null);
    try {
      const explained = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/segments/{id}/ai/explain", { params: { path: { ws: workspaceId, id: segmentId } } }),
      );
      if (mine === ticket.current) setRun({ status: "done", explained });
    } catch (error) {
      if (mine !== ticket.current) return;
      if (assistantGone(error)) onGone();
      setRun({ status: "failed", failure: failureText(error) });
    }
  }

  const root = fromDefinition(definition);
  const stale = shown !== null && shown.version !== version;
  return (
    <section className="dock-body" aria-label="Explain this segment">
      <div className="row">
        <Button variant={run.status === "done" ? "quiet" : "primary"} onClick={() => void explain()} disabled={run.status === "running"}>
          {run.status === "done" || run.status === "failed" ? "Explain again" : "Explain this segment"}
        </Button>
      </div>
      <p className="visually-hidden" role="status">
        {run.status === "running" ? "Asking the assistant" : run.status === "done" ? "Explanation ready" : ""}
      </p>
      {run.status === "running" ? <LoadingState label="Asking the assistant to explain this segment. This can take a minute." /> : null}
      {run.status === "failed" ? <InlineError {...run.failure} /> : null}
      {shown !== null ? (
        <div className="stack-tight">
          <h3 ref={heading} tabIndex={-1}>
            Version {shown.version}, as the assistant reads it
          </h3>
          {stale ? (
            <p className="form-error" role="alert">
              The segment changed since this explanation. It describes version {shown.version}; use Explain again.
            </p>
          ) : null}
          <p>{shown.summary}</p>
          <h4>Condition by condition</h4>
          <ol className="dock-explained" aria-label="Condition by condition">
            {shown.nodes.map((entry) => {
              const placement = place(root, entry.pointer);
              const node = placement ? getAt(root, placement.path) : undefined;
              return (
                <li key={entry.pointer}>
                  <button
                    type="button"
                    aria-pressed={selected === entry.pointer}
                    onClick={() => onSelect(selected === entry.pointer ? null : entry.pointer)}
                  >
                    {node ? <span className="strong">{describeNode(node, names)}</span> : null}
                    <span>{entry.text}</span>
                  </button>
                </li>
              );
            })}
          </ol>
          <p className="faint">Explained from the saved rules. Field names and rules only; no personal data was sent.</p>
        </div>
      ) : null}
    </section>
  );
}
