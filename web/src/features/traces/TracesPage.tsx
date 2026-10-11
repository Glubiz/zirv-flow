import { useState, type SyntheticEvent } from "react";
import { useNavigate, useParams } from "react-router";

import { ApiError, client, unwrap, useResource } from "../../api";
import { Button, EmptyState, formatDateTime, PageHeader, Panel, ResourceView, TextField } from "../../ui";

const TRACE_ID = /^[0-9a-f]{32}$/;
const LIMIT = 1000;
const INVALID = "A trace id is 32 lowercase letters a to f and digits 0 to 9.";

function TraceResult({ workspaceId, traceId }: { workspaceId: string; traceId: string }) {
  const trace = useResource(async () => {
    try {
      return await unwrap(
        client.GET("/api/v1/workspaces/{ws}/traces/{trace_id}", { params: { path: { ws: workspaceId, trace_id: traceId } } }),
      );
    } catch (error) {
      if (error instanceof ApiError && error.status === 404) {
        return null;
      }
      throw error;
    }
  }, [workspaceId, traceId]);

  return (
    <Panel flush>
      <ResourceView
        resource={trace}
        loadingLabel="Loading the trace"
        isEmpty={(data) => data === null || data.records.length === 0}
        empty={<EmptyState title="No records for this trace in this workspace" />}
      >
        {(data) => (
          <>
            {data?.truncated ? (
              <p className="muted state" role="status">
                Showing the first {LIMIT} records
              </p>
            ) : null}
            <div className="table-scroll">
              <table className="table">
                <caption>Records for trace {traceId}</caption>
                <thead>
                  <tr>
                    <th scope="col">Time</th>
                    <th scope="col">Action</th>
                    <th scope="col">Outcome</th>
                    <th scope="col">Cause</th>
                    <th scope="col">Job</th>
                    <th scope="col">Attempt</th>
                    <th scope="col">Reason</th>
                    <th scope="col">Subjects</th>
                  </tr>
                </thead>
                <tbody>
                  {data?.records.map((record) => (
                    <tr key={record.id}>
                      <td>{formatDateTime(record.recorded_at)}</td>
                      <td className="mono">{record.action}</td>
                      <td className="mono">{record.outcome}</td>
                      <td className="mono">
                        {record.cause.kind} {record.cause.id}
                      </td>
                      <td className="mono">{record.job_id ?? "none"}</td>
                      <td className="mono">{record.attempt ?? "none"}</td>
                      <td>{record.reason ?? ""}</td>
                      <td className="mono">
                        {Object.entries(record.subject_ids)
                          .map(([name, id]) => `${name}=${String(id)}`)
                          .join(" ")}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </>
        )}
      </ResourceView>
    </Panel>
  );
}

export function TracesPage() {
  const { ws = "", traceId } = useParams();
  const navigate = useNavigate();
  const [input, setInput] = useState(traceId ?? "");
  const [problem, setProblem] = useState<string | null>(null);

  const urlInvalid = traceId !== undefined && !TRACE_ID.test(traceId);
  const shown = problem ?? (urlInvalid ? INVALID : null);

  function submit(event: SyntheticEvent) {
    event.preventDefault();
    const value = input.trim();
    if (!TRACE_ID.test(value)) {
      setProblem(INVALID);
      return;
    }
    setProblem(null);
    void navigate(`/w/${ws}/traces/${value}`);
  }

  return (
    <div className="page">
      <PageHeader title="Traces" lede="Look up what was decided and done under one trace in this workspace." />
      <form className="inline-form" onSubmit={submit} noValidate>
        <TextField
          label="Trace ID"
          mono
          spellCheck={false}
          autoComplete="off"
          hint="Find it in the x-trace-id header of an API response, or on a job that needs attention."
          value={input}
          onChange={(event) => setInput(event.target.value)}
          aria-invalid={shown !== null}
        />
        <Button type="submit" variant="primary">
          Show trace
        </Button>
      </form>
      {shown ? (
        <p className="form-error" role="alert">
          {shown}
        </p>
      ) : null}
      {traceId !== undefined && !urlInvalid ? <TraceResult workspaceId={ws} traceId={traceId} /> : null}
    </div>
  );
}
