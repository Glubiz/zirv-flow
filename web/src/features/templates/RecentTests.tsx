import { useEffect, useState } from "react";

import { client, describeError, unwrap, type components, type ErrorSummary, type Resource } from "../../api";
import { Button, EmptyState, ErrorState, InlineError, LoadingState, Pill } from "../../ui";
import { formatDateTime } from "../../ui/format";
import type { TestSend } from "./model";
import { statusOf } from "./testSendStatus";

type Page = components["schemas"]["TestSendsBody"];

export const LOG_PAGE = 20;
/** How long the log waits before it asks again while a test is still queued. */
export const POLL_MS = 3000;

interface Paged {
  key: number;
  rows: TestSend[];
  next: string | null | undefined;
}

interface Props {
  workspaceId: string;
  templateId: string;
  /** The newest page, read by the page so that a send can reload it. */
  log: Resource<Page>;
  /** Changes whenever the newest rows may have moved; older pages are dropped then. */
  resetKey: number;
}

/** The recent tests of one template: When, To, Content and Result, newest first, with older pages on request. */
export function RecentTests({ workspaceId, templateId, log, resetKey }: Props) {
  const [paged, setPaged] = useState<Paged>({ key: resetKey, rows: [], next: undefined });
  const { rows: older, next } = paged.key === resetKey ? paged : { rows: [], next: undefined };
  const [loading, setLoading] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  const data = log.status === "ready" ? log.data : null;
  const queued = data?.test_sends.some((row) => row.outcome === "queued") ?? false;
  const reload = log.reload;
  useEffect(() => {
    if (!queued) return;
    const timer = window.setTimeout(reload, POLL_MS);
    return () => window.clearTimeout(timer);
  }, [queued, data, reload]);

  async function showOlder(cursor: string) {
    setLoading(true);
    setFailure(null);
    try {
      const page = await unwrap(
        client.GET("/api/v1/workspaces/{ws}/test-sends", {
          params: { path: { ws: workspaceId }, query: { template_id: templateId, limit: LOG_PAGE, before: cursor } },
        }),
      );
      setPaged({ key: resetKey, rows: [...older, ...page.test_sends], next: page.next_before });
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setLoading(false);
    }
  }

  return (
    <section className="panel panel-pad ts-log" aria-labelledby="ts-log-title">
      <div className="row">
        <h2 className="panel-title" id="ts-log-title">
          Recent tests
        </h2>
        <span className="faint row-grow ts-kept">Kept for 13 months</span>
      </div>
      {log.status === "loading" ? <LoadingState label="Loading the recent tests" /> : null}
      {log.status === "error" ? <ErrorState {...describeError(log.error)} onRetry={log.reload} /> : null}
      {log.status === "ready" ? (
        <Rows rows={[...log.data.test_sends, ...older]} more={next === undefined ? log.data.next_before : next} loading={loading} onMore={(cursor) => void showOlder(cursor)} />
      ) : null}
      {failure ? <InlineError {...failure} /> : null}
    </section>
  );
}

function Rows({ rows, more, loading, onMore }: { rows: TestSend[]; more: string | null | undefined; loading: boolean; onMore: (cursor: string) => void }) {
  if (rows.length === 0) return <EmptyState title="No tests sent yet">Pick who gets one and send it.</EmptyState>;
  return (
    <>
      <div className="table-scroll">
        <table className="table">
          <caption className="visually-hidden">Recent tests of this template</caption>
          <thead>
            <tr>
              <th scope="col">When</th>
              <th scope="col">To</th>
              <th scope="col">Content</th>
              <th scope="col">Result</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((row) => {
              const status = statusOf(row);
              return (
                <tr key={row.id}>
                  <td className="ts-when">{formatDateTime(row.created_at)}</td>
                  <td>
                    <div className="strong">{row.recipient_display}</div>
                    {row.sent_by_email ? <div className="faint">by {row.sent_by_email}</div> : null}
                  </td>
                  <td>
                    {row.template_version ? `Version ${row.template_version}` : "Draft"}
                    <div className="faint">as {row.render_context === "person" ? "a saved person" : "the sample person"}</div>
                  </td>
                  <td>
                    <Pill tone={status.tone}>{status.label}</Pill>
                    {status.detail ? <div className="faint">{status.detail}</div> : null}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      {more ? (
        <Button onClick={() => onMore(more)} disabled={loading}>
          {loading ? "Loading" : "Show older tests"}
        </Button>
      ) : null}
    </>
  );
}
