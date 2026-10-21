import { useState } from "react";

import { client, unwrap, type components } from "../../api";
import { Button, InlineError, LoadingState } from "../../ui";
import { failureText, type Failure } from "../assistant";

type Explained = components["schemas"]["AiTraceExplainBody"];

type Run =
  | { status: "idle" }
  | { status: "running" }
  | { status: "failed"; failure: Failure }
  | { status: "done"; explained: Explained };

interface Props {
  workspaceId: string;
  traceId: string;
  /** What a record is in words, such as the action, the outcome and the step; null when it is not among the records shown. */
  recordLabel: (id: string) => string | null;
  /** The records the explanation cites, to be marked in the trace tree; empty before it and after a new ask. */
  onCited: (ids: readonly string[]) => void;
  /** Scrolls to a record in the trace tree and highlights it. */
  onShow: (id: string) => void;
}

/**
 * The assistant's explanation of the trace of one enrollment, in the inspector. The sentences are
 * plain text. Each cites a decision record, and a button takes the reader to that record's row.
 */
export function TraceExplanation({ workspaceId, traceId, recordLabel, onCited, onShow }: Props) {
  const [run, setRun] = useState<Run>({ status: "idle" });

  async function explain() {
    if (run.status === "running") return;
    setRun({ status: "running" });
    onCited([]);
    try {
      const explained = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/traces/{trace_id}/ai/explain", {
          params: { path: { ws: workspaceId, trace_id: traceId } },
        }),
      );
      setRun({ status: "done", explained });
      onCited(explained.records.map((entry) => entry.id));
    } catch (error) {
      setRun({ status: "failed", failure: failureText(error) });
    }
  }

  return (
    <section className="stack-tight" aria-label="Explanation by the assistant">
      <div className="row">
        <Button onClick={() => void explain()} disabled={run.status === "running"}>
          {run.status === "done" || run.status === "failed" ? "Explain again" : "Explain this trace"}
        </Button>
      </div>
      <p className="visually-hidden" role="status">
        {run.status === "running" ? "Asking the assistant" : run.status === "done" ? "Explanation ready" : ""}
      </p>
      {run.status === "running" ? <LoadingState label="Asking the assistant to explain this trace. This can take a minute." /> : null}
      {run.status === "failed" ? <InlineError {...run.failure} /> : null}
      {run.status === "done" ? (
        <div className="stack-tight">
          <p className="faint">Written by the assistant. Check it against the decisions: each sentence names the one it describes.</p>
          <p>{run.explained.summary}</p>
          {run.explained.omitted_records > 0 ? (
            <p className="muted">
              The trace is long: the assistant did not see the last {run.explained.omitted_records} records.
            </p>
          ) : null}
          {run.explained.records.length === 0 ? (
            <p className="muted">The assistant cited no decisions.</p>
          ) : (
            <ol className="list-plain fl-cites" aria-label="Decision by decision">
              {run.explained.records.map((entry) => {
                const label = recordLabel(entry.id);
                return (
                  <li key={entry.id} className="fl-cite">
                    {label !== null ? (
                      <Button onClick={() => onShow(entry.id)}>Show the decision: {label}</Button>
                    ) : (
                      <span className="muted">A decision that is not in the list below</span>
                    )}
                    <span>{entry.text}</span>
                  </li>
                );
              })}
            </ol>
          )}
        </div>
      ) : null}
    </section>
  );
}
