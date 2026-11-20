import { useEffect, useRef, useState } from "react";

import { client, describeError, unwrap, useResource, type ErrorSummary } from "../../api";
import { Button, EmptyState, formatDateTime, InlineError, Pill, ResourceView } from "../../ui";
import { isActiveRun, runKindLabel, runStateLabel, runStateTone, triggerLabel, type Run } from "./labels";
import { RunDialog } from "./RunDialog";

export const RUNS_POLL_MS = 3000;
const PAGE = 25;

interface RunsPanelProps {
  workspaceId: string;
  sourceId: string;
  canManage: boolean;
  /** Changes when a run was started elsewhere on the page, so the history loads again. */
  version: number;
  pollMs?: number;
  /** Called once when the last active run of the history ends, so the page can refresh what a run changes. */
  onSettled?: () => void;
}

/** The run history of a source: state, attempt, times and counters, with cancel for admins. */
export function RunsPanel({ workspaceId, sourceId, canManage, version, pollMs = RUNS_POLL_MS, onSettled }: RunsPanelProps) {
  const resource = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/sources/{id}/runs", {
          params: { path: { ws: workspaceId, id: sourceId }, query: { limit: PAGE } },
        }),
      ),
    [workspaceId, sourceId, version],
  );
  const key = `${workspaceId}/${sourceId}/${version}`;
  // Older pages belong to the history they were loaded for; a new key starts again from the newest.
  const [extra, setExtra] = useState<{ key: string; runs: Run[]; cursor: string | null } | null>(null);
  const older = extra?.key === key ? extra : null;
  // The first page last seen: when a newer run shifts it, the rows it pushed out are kept with the older ones.
  const [seen, setSeen] = useState<{ key: string; page: object; runs: Run[] } | null>(null);
  const [viewing, setViewing] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  const active = resource.status === "ready" && resource.data.runs.some(isActiveRun);
  const reload = resource.reload;
  if (resource.status === "ready" && seen?.page !== resource.data) {
    if (older && seen?.key === key) {
      const now = new Set(resource.data.runs.map((run) => run.id));
      const had = new Set(older.runs.map((run) => run.id));
      const pushed = seen.runs.filter((run) => !now.has(run.id) && !had.has(run.id));
      if (pushed.length > 0) setExtra({ ...older, runs: [...pushed, ...older.runs] });
    }
    setSeen({ key, page: resource.data, runs: resource.data.runs });
  }
  const was = useRef(false);
  const settled = useRef(onSettled);
  useEffect(() => {
    settled.current = onSettled;
  });
  useEffect(() => {
    if (was.current && !active) settled.current?.();
    was.current = active;
  }, [active]);
  useEffect(() => {
    if (!active) return;
    const timer = setInterval(reload, pollMs);
    return () => clearInterval(timer);
  }, [active, reload, pollMs]);

  async function cancel(run: Run) {
    setBusy(true);
    setFailure(null);
    try {
      const cancelled = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/sources/{id}/runs/{run_id}/cancel", {
          params: { path: { ws: workspaceId, id: sourceId, run_id: run.id } },
        }),
      );
      // Only the first page reloads, so an older row is patched from the answer.
      setExtra((current) => (current ? { ...current, runs: current.runs.map((row) => (row.id === cancelled.id ? cancelled : row)) } : current));
      reload();
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  async function loadOlder(next: string) {
    setBusy(true);
    setFailure(null);
    try {
      const page = await unwrap(
        client.GET("/api/v1/workspaces/{ws}/sources/{id}/runs", {
          params: { path: { ws: workspaceId, id: sourceId }, query: { limit: PAGE, cursor: next } },
        }),
      );
      setExtra((current) => ({
        key,
        runs: [...(current?.key === key ? current.runs : []), ...page.runs],
        cursor: page.next_cursor ?? null,
      }));
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <>
      <ResourceView
        resource={resource}
        loadingLabel="Loading runs"
        isEmpty={(data) => data.runs.length === 0}
        empty={
          <EmptyState title="No runs yet">
            {canManage
              ? "Run a stream now, or give the source a schedule, to read it for the first time."
              : "Nothing has been read from this source yet."}
          </EmptyState>
        }
      >
        {(data) => {
          const known = new Set(data.runs.map((run) => run.id));
          const rows = [...data.runs, ...(older?.runs ?? []).filter((run) => !known.has(run.id))];
          const next = older ? older.cursor : data.next_cursor;
          return (
            <>
              {failure ? <InlineError {...failure} /> : null}
              <div className="table-scroll">
                <table className="table">
                  <caption>Runs of this source</caption>
                  <thead>
                    <tr>
                      <th scope="col">Stream</th>
                      <th scope="col">State</th>
                      <th scope="col">Kind</th>
                      <th scope="col">Started by</th>
                      <th scope="col" className="num">
                        Attempt
                      </th>
                      <th scope="col">Started</th>
                      <th scope="col">Finished</th>
                      <th scope="col" className="num">
                        Read
                      </th>
                      <th scope="col" className="num">
                        Applied
                      </th>
                      <th scope="col" className="num">
                        Refused
                      </th>
                      <th scope="col" className="num">
                        Deleted
                      </th>
                      <th scope="col">
                        <span className="visually-hidden">Actions</span>
                      </th>
                    </tr>
                  </thead>
                  <tbody>
                    {rows.map((run) => (
                      <tr key={run.id}>
                        <td className="strong">{run.stream}</td>
                        <td>
                          <Pill tone={runStateTone(run.state)}>{runStateLabel(run.state)}</Pill>
                        </td>
                        <td>{runKindLabel(run.kind)}</td>
                        <td>{triggerLabel(run.trigger)}</td>
                        <td className="num">{run.attempt}</td>
                        <td>{run.started_at ? formatDateTime(run.started_at) : "Not yet"}</td>
                        <td>{run.finished_at ? formatDateTime(run.finished_at) : "—"}</td>
                        <td className="num">{run.records_read}</td>
                        <td className="num">{run.records_applied}</td>
                        <td className="num">{run.records_rejected}</td>
                        <td className="num">{run.records_deleted}</td>
                        <td className="num">
                          <Button onClick={() => setViewing(run.id)} aria-label={`Details of the ${run.stream} run`}>
                            Details
                          </Button>
                          {canManage && isActiveRun(run) ? (
                            <Button
                              onClick={() => void cancel(run)}
                              disabled={busy}
                              aria-label={`Cancel the ${run.stream} run`}
                            >
                              Cancel
                            </Button>
                          ) : null}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
              {next ? (
                <div className="panel-footer">
                  <Button onClick={() => void loadOlder(next)} disabled={busy}>
                    Show older runs
                  </Button>
                </div>
              ) : null}
            </>
          );
        }}
      </ResourceView>
      {viewing ? (
        <RunDialog
          key={viewing}
          workspaceId={workspaceId}
          sourceId={sourceId}
          runId={viewing}
          pollMs={pollMs}
          onClose={() => setViewing(null)}
        />
      ) : null}
    </>
  );
}
