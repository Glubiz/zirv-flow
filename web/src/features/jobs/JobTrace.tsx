import { client, unwrap, useResource } from "../../api";
import { EmptyState, ResourceView } from "../../ui";
import type { DeadJob } from "./useDeadJobs";

export function JobTrace({ job }: { job: DeadJob }) {
  const workspaceId = job.workspace_id ?? null;
  const trace = useResource(async () => {
    if (workspaceId === null) return null;
    return unwrap(
      client.GET("/api/v1/workspaces/{ws}/traces/{trace_id}", {
        params: { path: { ws: workspaceId, trace_id: job.trace_id } },
      }),
    );
  }, [workspaceId, job.trace_id]);

  return (
    <div className="stack-tight">
      <p className="muted">
        Trace ID <span className="mono">{job.trace_id}</span>
      </p>
      {job.last_error ? (
        <p className="muted">
          Last error: <span className="mono">{job.last_error}</span>
        </p>
      ) : null}
      {workspaceId === null ? (
        <p className="muted">This job does not belong to a workspace, so it has no decision record.</p>
      ) : (
        <ResourceView
          resource={trace}
          loadingLabel="Loading what happened"
          isEmpty={(data) => data === null || data.records.length === 0}
          empty={<EmptyState title="Nothing was recorded under this trace" />}
        >
          {(data) => (
            <ul className="records list-plain" aria-label="What happened">
              {data?.records.map((record) => (
                <li key={record.id}>
                  <span className="mono">{record.action}</span> {record.outcome}
                  {record.reason ? <span className="muted"> {record.reason}</span> : null}
                </li>
              ))}
              {data?.truncated ? <li className="muted">Only the first records are shown.</li> : null}
            </ul>
          )}
        </ResourceView>
      )}
    </div>
  );
}
