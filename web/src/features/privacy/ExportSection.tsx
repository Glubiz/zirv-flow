import { useCallback, useEffect, useState } from "react";

import { client, describeError, unwrap, type components, type ErrorSummary } from "../../api";
import {
  EmptyState,
  formatDateTime,
  formatDuration,
  InlineError,
  Panel,
  Pill,
  ResourceView,
  shortId,
} from "../../ui";
import { exportStatus, secondsLeft } from "./labels";
import { TargetForm, type PersonTarget } from "./TargetForm";
import { ShowMore } from "./ShowMore";
import { usePaged } from "./usePaged";
import { usePolling } from "./usePolling";

type Export = components["schemas"]["ExportBody"];

const PAGE_SIZE = 50;
const TICK_MS = 30_000;

/** The current time, refreshed every half minute so a countdown stays honest. */
function useNow(): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), TICK_MS);
    return () => clearInterval(timer);
  }, []);
  return now;
}

export function ExportSection({ workspaceId }: { workspaceId: string }) {
  const fetchPage = useCallback(
    async (before: string | null) => {
      const page = await unwrap(
        client.GET("/api/v1/workspaces/{ws}/privacy/exports", {
          params: { path: { ws: workspaceId }, query: { limit: PAGE_SIZE, before: before ?? undefined } },
        }),
      );
      return { items: page.exports, next: page.next_before ?? null };
    },
    [workspaceId],
  );
  const exports = usePaged(fetchPage, [workspaceId]);
  usePolling(
    exports.resource.status === "ready" && exports.resource.data.some((item) => item.status === "pending"),
    exports.resource.status === "ready" && exports.resource.refreshing,
    exports.resource.reload,
  );
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const now = useNow();

  async function request(target: PersonTarget) {
    setBusy(true);
    setFailure(null);
    setNotice(null);
    try {
      await unwrap(
        client.POST("/api/v1/workspaces/{ws}/privacy/exports", { params: { path: { ws: workspaceId } }, body: target }),
      );
      setNotice("Export requested. It appears below when it is ready, and can be downloaded for 24 hours.");
      exports.resource.reload();
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <>
      <Panel>
        <p className="muted">
          An export holds everything stored about a person: profile, identities, events, consent, lists and their history.
          It contains personal data, so only workspace admins can request or download one, every download is recorded, and
          it can be downloaded for 24 hours.
        </p>
        <TargetForm legend="Export a person" submitLabel="Request an export" busy={busy} onSubmit={(target) => void request(target)} />
        {failure ? <InlineError {...failure} /> : null}
        {notice ? (
          <p className="notice" role="status">
            {notice}
          </p>
        ) : null}
      </Panel>
      <Panel title="Exports" flush>
        {exports.refreshError ? <InlineError {...describeError(exports.refreshError)} /> : null}
        <ResourceView
          resource={exports.resource}
          loadingLabel="Loading exports"
          isEmpty={(data) => data.length === 0}
          empty={<EmptyState title="No export has been requested">Requested exports appear here with their expiry.</EmptyState>}
        >
          {(data) => (
            <div className="table-scroll">
              <table className="table">
                <caption>Exports</caption>
                <thead>
                  <tr>
                    <th scope="col">Person</th>
                    <th scope="col">Status</th>
                    <th scope="col">Requested</th>
                    <th scope="col">Expires</th>
                    <th scope="col">
                      <span className="visually-hidden">Download</span>
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {data.map((item) => (
                    <ExportRow key={item.id} workspaceId={workspaceId} item={item} now={now} />
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </ResourceView>
        <ShowMore paged={exports} />
      </Panel>
    </>
  );
}

function ExportRow({ workspaceId, item, now }: { workspaceId: string; item: Export; now: number }) {
  const left = item.expires_at ? secondsLeft(item.expires_at, now) : null;
  // A ready export whose time ran out since the list loaded reads as expired.
  const effective = item.status === "ready" && left === 0 ? "expired" : item.status;
  const status = exportStatus(effective);
  const downloadable = effective === "ready";

  return (
    <tr>
      <td className="mono" title={item.person_id}>
        {shortId(item.person_id)}
      </td>
      <td>
        <Pill tone={status.tone}>{status.label}</Pill>
        {effective === "failed" ? (
          <div className="faint">
            Trace ID <span className="mono">{item.trace_id}</span>
          </div>
        ) : null}
      </td>
      <td>{formatDateTime(item.requested_at)}</td>
      <td>
        {downloadable && left !== null ? `In ${formatDuration(left)}` : item.expires_at && effective === "expired" ? "Expired" : "—"}
      </td>
      <td>
        {downloadable ? (
          <a
            className="btn"
            href={`/api/v1/workspaces/${workspaceId}/privacy/exports/${item.id}/download`}
            download={`person-export-${item.id}.json.gz`}
            aria-label={`Download the export of ${shortId(item.person_id)}`}
          >
            Download
          </a>
        ) : null}
      </td>
    </tr>
  );
}
