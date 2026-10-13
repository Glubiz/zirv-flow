import { useEffect } from "react";
import { Link } from "react-router";

import { client, unwrap, useResource } from "../../api";
import { Button, Dialog, EmptyState, formatDateTime, Pill, ResourceView } from "../../ui";
import { traceHref } from "../traces";
import {
  isActiveRun,
  plural,
  runErrorText,
  runKindLabel,
  runStateLabel,
  runStateTone,
  showValue,
  syncModeLabel,
  triggerLabel,
  type RunReject,
} from "./labels";

interface RunDialogProps {
  workspaceId: string;
  sourceId: string;
  runId: string;
  pollMs: number;
  onClose: () => void;
}

/** The sample of a refused record: field names, and a value only where it is safe to show. */
function Sample({ reject }: { reject: RunReject }) {
  const sample = reject.sample as { properties?: Record<string, unknown> };
  const properties = Object.entries(sample.properties ?? {});
  if (properties.length === 0) return <span className="faint">Nothing to show</span>;
  return (
    <dl className="facts">
      {properties.map(([name, value]) => (
        <div key={name} className="fact">
          <dt className="mono">{name}</dt>
          <dd>{value === null ? <span className="faint">Hidden</span> : showValue(value)}</dd>
        </div>
      ))}
    </dl>
  );
}

/** One run: its counters, why it failed, the records it refused and a link to its trace. */
export function RunDialog({ workspaceId, sourceId, runId, pollMs, onClose }: RunDialogProps) {
  const resource = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/sources/{id}/runs/{run_id}", {
          params: { path: { ws: workspaceId, id: sourceId, run_id: runId } },
        }),
      ),
    [workspaceId, sourceId, runId],
  );
  const active = resource.status === "ready" && isActiveRun(resource.data);
  const reload = resource.reload;
  useEffect(() => {
    if (!active) return;
    const timer = setInterval(reload, pollMs);
    return () => clearInterval(timer);
  }, [active, reload, pollMs]);

  return (
    <Dialog title="Run details" onClose={onClose} wide>
      <ResourceView resource={resource} loadingLabel="Loading the run">
        {(run) => (
          <div className="stack">
            <p>
              <span className="strong">{run.stream}</span>{" "}
              <Pill tone={runStateTone(run.state)}>{runStateLabel(run.state)}</Pill>
            </p>
            <dl className="facts">
              <div className="fact">
                <dt>Kind</dt>
                <dd>{runKindLabel(run.kind)}</dd>
              </div>
              <div className="fact">
                <dt>Started by</dt>
                <dd>{triggerLabel(run.trigger)}</dd>
              </div>
              <div className="fact">
                <dt>Sync mode</dt>
                <dd>{syncModeLabel(run.sync_mode)}</dd>
              </div>
              <div className="fact">
                <dt>Attempt</dt>
                <dd>{run.attempt}</dd>
              </div>
              <div className="fact">
                <dt>Started</dt>
                <dd>{run.started_at ? formatDateTime(run.started_at) : "Not yet"}</dd>
              </div>
              <div className="fact">
                <dt>Finished</dt>
                <dd>{run.finished_at ? formatDateTime(run.finished_at) : "Not yet"}</dd>
              </div>
              <div className="fact">
                <dt>Records read</dt>
                <dd>{run.records_read}</dd>
              </div>
              <div className="fact">
                <dt>Applied</dt>
                <dd>{run.records_applied}</dd>
              </div>
              <div className="fact">
                <dt>Unchanged</dt>
                <dd>{run.records_unchanged}</dd>
              </div>
              <div className="fact">
                <dt>Refused</dt>
                <dd>{run.records_rejected}</dd>
              </div>
              <div className="fact">
                <dt>Removed records</dt>
                <dd>{run.records_deleted}</dd>
              </div>
            </dl>
            <p className="muted">
              Removed records are ones the source no longer lists, or marks as deleted. Their events go with them.
            </p>
            {run.attempt > 1 ? (
              <p className="muted">
                An earlier attempt was lost, so this run picked up from its last saved point without applying anything
                twice.
              </p>
            ) : null}
            {run.error_code ? (
              <p className="form-error" role="alert">
                <span className="strong">Why it failed</span>
                <span>{runErrorText(run.error_code, run.reject_threshold)}</span>
                <span className="faint mono">{run.error_code}</span>
              </p>
            ) : null}
            <p>
              <Link to={traceHref(workspaceId, run.trace_id)}>View trace</Link>
            </p>
            <h3 className="panel-title">Refused records</h3>
            {run.rejects.length === 0 ? (
              <EmptyState title="No refused records">Every record this run read was accepted.</EmptyState>
            ) : (
              <>
                <div className="table-scroll">
                  <table className="table">
                    <caption>Records this run refused</caption>
                    <thead>
                      <tr>
                        <th scope="col">Time</th>
                        <th scope="col">Reasons</th>
                        <th scope="col">Sample</th>
                      </tr>
                    </thead>
                    <tbody>
                      {run.rejects.map((reject) => (
                        <tr key={reject.id}>
                          <td>{formatDateTime(reject.received_at)}</td>
                          <td>
                            {reject.reasons.map((reason, index) => (
                              <div key={index} className="mono">
                                {reason.property ? `${reason.property}: ${reason.reason}` : reason.reason}
                              </div>
                            ))}
                          </td>
                          <td>
                            <Sample reject={reject} />
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
                {run.records_rejected > run.rejects.length ? (
                  <p className="muted">
                    Showing the first {plural(run.rejects.length, "refused record", "refused records")} of{" "}
                    {run.records_rejected}.
                  </p>
                ) : null}
              </>
            )}
            <div className="dialog-actions">
              <Button onClick={onClose}>Close</Button>
            </div>
          </div>
        )}
      </ResourceView>
    </Dialog>
  );
}
