import { useState } from "react";

import { ApiError, client, describeError, unwrap, useResource } from "../../api";
import {
  Button,
  EmptyState,
  ErrorState,
  formatDuration,
  InlineError,
  LoadingState,
  PageHeader,
  Panel,
  ResourceView,
  shortId,
} from "../../ui";
import { useSignedInUser } from "../session";
import { JobTrace } from "./JobTrace";
import { useDeadJobs, type DeadJob } from "./useDeadJobs";

type Notice = { tone: "ok" | "problem"; text: string };

function retryOutcome(error: unknown, job: DeadJob): string {
  if (error instanceof ApiError && error.status === 404) {
    return `The ${job.kind} job ${shortId(job.id)} is no longer waiting to be retried. It may already have been put back.`;
  }
  if (error instanceof ApiError && error.status === 409) {
    return `The ${job.kind} job ${shortId(job.id)} was not queued again because an equivalent job is already waiting.`;
  }
  const summary = describeError(error);
  return `Could not retry the ${job.kind} job ${shortId(job.id)}. ${summary.detail || summary.title}`;
}

export function JobsPage() {
  const user = useSignedInUser();
  if (!user.is_owner) {
    return (
      <div className="page">
        <PageHeader title="Jobs" />
        <Panel flush>
          <EmptyState title="Only the instance owner can see jobs">
            Background work is shared across every workspace, so it is managed by the owner of this instance.
          </EmptyState>
        </Panel>
      </div>
    );
  }
  return <JobsAdmin />;
}

function JobsAdmin() {
  const depth = useResource(() => unwrap(client.GET("/api/v1/admin/jobs")), []);
  const dead = useDeadJobs();
  const [retrying, setRetrying] = useState<string | null>(null);
  const [notice, setNotice] = useState<Notice | null>(null);
  const [open, setOpen] = useState<string | null>(null);

  function refresh() {
    depth.reload();
    dead.reload();
  }

  async function retry(job: DeadJob) {
    setRetrying(job.id);
    setNotice(null);
    try {
      await unwrap(client.POST("/api/v1/admin/jobs/{id}/retry", { params: { path: { id: job.id } } }));
      setNotice({ tone: "ok", text: `The ${job.kind} job ${shortId(job.id)} is back in the queue and will run again shortly.` });
      refresh();
    } catch (error) {
      setNotice({ tone: "problem", text: retryOutcome(error, job) });
      if (error instanceof ApiError && error.status === 404) {
        refresh();
      }
    } finally {
      setRetrying(null);
    }
  }

  const kinds = depth.status === "ready" ? depth.data.kinds : [];
  const waiting = kinds.reduce((sum, kind) => sum + kind.ready, 0);
  const running = kinds.reduce((sum, kind) => sum + kind.running, 0);
  const stopped = kinds.reduce((sum, kind) => sum + kind.dead, 0);
  const longest = kinds.reduce<number | null>(
    (best, kind) => (kind.oldest_ready_seconds === null || kind.oldest_ready_seconds === undefined ? best : Math.max(best ?? 0, kind.oldest_ready_seconds)),
    null,
  );
  const stat = (value: number | string) => (depth.status === "ready" ? value : "–");

  return (
    <div className="page">
      <PageHeader
        title="Jobs"
        lede="Background work across every workspace. Retry anything that ran out of attempts."
        actions={<Button onClick={refresh}>Refresh</Button>}
      />
      <section aria-label="Totals" className="grid-4">
        <div className="panel stat">
          <span className="stat-label">Waiting</span>
          <span className="stat-value">{stat(waiting)}</span>
        </div>
        <div className="panel stat">
          <span className="stat-label">Running now</span>
          <span className="stat-value">{stat(running)}</span>
        </div>
        <div className={stopped > 0 ? "panel stat stat-attention" : "panel stat"}>
          <span className="stat-label">
            {stopped > 0 ? <span className="dot dot-attention" aria-hidden="true" /> : null}Need attention
          </span>
          <span className="stat-value">{stat(stopped)}</span>
        </div>
        <div className="panel stat">
          <span className="stat-label">Longest wait</span>
          <span className="stat-value">{depth.status === "ready" ? (longest === null ? "None" : formatDuration(longest)) : "–"}</span>
        </div>
      </section>
      <div className="split">
        <section className="panel panel-flush grow-1" aria-labelledby="by-kind">
          <h2 className="panel-title" id="by-kind">
            By kind
          </h2>
          <ResourceView
            resource={depth}
            loadingLabel="Loading job counts"
            isEmpty={(data) => data.kinds.length === 0}
            empty={<EmptyState title="No jobs have run yet">Counts per kind appear here once the first job is queued.</EmptyState>}
          >
            {(data) => (
              <div className="table-scroll">
                <table className="table">
                  <caption>Jobs by kind</caption>
                  <thead>
                    <tr>
                      <th scope="col">Kind</th>
                      <th scope="col" className="num">
                        Waiting
                      </th>
                      <th scope="col" className="num">
                        Running
                      </th>
                      <th scope="col" className="num">
                        Dead
                      </th>
                      <th scope="col" className="num">
                        Oldest waiting
                      </th>
                    </tr>
                  </thead>
                  <tbody>
                    {data.kinds.map((kind) => (
                      <tr key={kind.kind}>
                        <td className="mono">{kind.kind}</td>
                        <td className="num">{kind.ready}</td>
                        <td className="num">{kind.running}</td>
                        <td className="num">
                          {kind.dead > 0 ? (
                            <span className="attention">
                              <span className="dot dot-attention" aria-hidden="true" /> {kind.dead}
                            </span>
                          ) : (
                            kind.dead
                          )}
                        </td>
                        <td className="num">{formatDuration(kind.oldest_ready_seconds)}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </ResourceView>
        </section>
        <section className="panel panel-pad grow-2" aria-labelledby="attention">
          <h2 className="panel-title" id="attention">
            Need attention
          </h2>
          {notice ? (
            notice.tone === "ok" ? (
              <p className="muted" role="status">
                {notice.text}
              </p>
            ) : (
              <div role="status">
                <InlineError title="Not retried" detail={notice.text} />
              </div>
            )
          ) : null}
          {dead.status === "loading" ? <LoadingState label="Loading jobs that need attention" /> : null}
          {dead.status === "error" ? <ErrorState {...describeError(dead.error)} onRetry={dead.reload} /> : null}
          {dead.status === "ready" && dead.jobs.length === 0 ? (
            <EmptyState title="Nothing needs attention">Every job has either finished or is still being retried.</EmptyState>
          ) : null}
          {dead.status === "ready" && dead.jobs.length > 0 ? (
            <ul className="stack-tight list-plain">
              {dead.jobs.map((job) => (
                <li key={job.id} className="row-card">
                  <div className="row">
                    <div className="row-grow">
                      <span className="strong">
                        <span className="mono">{job.kind}</span> stopped after {job.attempts} {job.attempts === 1 ? "attempt" : "attempts"}
                      </span>
                      <span className="muted">
                        {job.last_error ? `${job.last_error} · ` : ""}job <span className="mono">{shortId(job.id)}</span>
                      </span>
                    </div>
                    <button
                      type="button"
                      className="link-button"
                      aria-expanded={open === job.id}
                      onClick={() => setOpen(open === job.id ? null : job.id)}
                    >
                      See what happened<span className="visually-hidden"> to {job.kind} job {shortId(job.id)}</span>
                    </button>
                    <Button
                      variant="primary"
                      disabled={retrying === job.id}
                      onClick={() => void retry(job)}
                    >
                      {retrying === job.id ? "Trying again" : "Try again"}
                      <span className="visually-hidden"> for {job.kind} job {shortId(job.id)}</span>
                    </Button>
                  </div>
                  {open === job.id ? (
                    <div className="stack-tight">
                      <JobTrace job={job} />
                    </div>
                  ) : null}
                </li>
              ))}
            </ul>
          ) : null}
          {dead.moreError ? <InlineError {...describeError(dead.moreError)} /> : null}
          {dead.nextBefore !== null && dead.status === "ready" ? (
            <div>
              <Button onClick={dead.loadMore} disabled={dead.loadingMore}>
                {dead.loadingMore ? "Loading" : "Load more"}
              </Button>
            </div>
          ) : null}
        </section>
      </div>
    </div>
  );
}
