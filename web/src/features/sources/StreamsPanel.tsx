import { useState } from "react";

import { Button, EmptyState, formatDateTime, Pill } from "../../ui";
import { StreamDialog } from "./StreamDialog";
import { FreshnessBadge } from "./FreshnessBadge";
import { lastSuccessLabel, syncModeLabel, type Stream } from "./labels";

interface StreamsPanelProps {
  workspaceId: string;
  sourceId: string;
  streams: Stream[];
  canManage: boolean;
  onChanged: (stream: Stream) => void;
  /** Starts a run of one stream; without it the table offers no run. */
  onRun?: (stream: Stream) => void;
  /** Starts a reconciliation of one incremental stream; without it the table offers none. */
  onReconcile?: (stream: Stream) => void;
  busy?: boolean;
}

/** The threshold in force, in seconds, marked when it is the default; the stream's own setting while it is not read. */
function stalenessCell(stream: Stream): string {
  const threshold = stream.freshness ? stream.freshness.threshold_seconds : stream.staleness_threshold_seconds;
  if (threshold === null || threshold === undefined) return "None";
  return stream.freshness?.threshold_is_default ? `${threshold} (default)` : String(threshold);
}

/** The streams of a source with their settings; admins set each one up in a dialog. */
export function StreamsPanel({ workspaceId, sourceId, streams, canManage, onChanged, onRun, onReconcile, busy }: StreamsPanelProps) {
  const [editing, setEditing] = useState<Stream | null>(null);

  if (streams.length === 0) {
    return (
      <EmptyState title="No streams yet">
        {canManage
          ? "Discover the streams this source offers, then choose which to read."
          : "An admin has not discovered the streams of this source yet."}
      </EmptyState>
    );
  }
  return (
    <>
      <div className="table-scroll">
        <table className="table">
          <caption>Streams of this source</caption>
          <thead>
            <tr>
              <th scope="col">Stream</th>
              <th scope="col">Status</th>
              <th scope="col">Freshness</th>
              <th scope="col">Last success</th>
              <th scope="col">Sync mode</th>
              <th scope="col">Cursor field</th>
              <th scope="col">Primary key</th>
              <th scope="col" className="num">
                Staleness (s)
              </th>
              {canManage ? (
                <th scope="col">
                  <span className="visually-hidden">Actions</span>
                </th>
              ) : null}
            </tr>
          </thead>
          <tbody>
            {streams.map((stream) => (
              <tr key={stream.id}>
                <td className="strong">{stream.name}</td>
                <td>
                  <Pill tone={stream.enabled ? "neutral" : "attention"}>{stream.enabled ? "Read" : "Not read"}</Pill>
                </td>
                <td>
                  <FreshnessBadge state={stream.freshness?.state} />
                </td>
                <td title={stream.freshness?.last_success_at ? formatDateTime(stream.freshness.last_success_at) : undefined}>
                  {stream.freshness ? lastSuccessLabel(stream.freshness) : "—"}
                </td>
                <td>{syncModeLabel(stream.sync_mode)}</td>
                <td>{stream.cursor_field ?? "None"}</td>
                <td>{stream.primary_key.length > 0 ? stream.primary_key.join(", ") : "None"}</td>
                <td className="num">{stalenessCell(stream)}</td>
                {canManage ? (
                  <td className="num">
                    {onRun && stream.enabled && stream.mapping ? (
                      <Button onClick={() => onRun(stream)} disabled={busy === true} aria-label={`Run ${stream.name} now`}>
                        Run now
                      </Button>
                    ) : null}
                    {onReconcile && stream.enabled && stream.mapping && stream.sync_mode === "incremental" ? (
                      <Button
                        onClick={() => onReconcile(stream)}
                        disabled={busy === true}
                        aria-label={`Reconcile ${stream.name} now`}
                      >
                        Reconcile now
                      </Button>
                    ) : null}
                    <Button onClick={() => setEditing(stream)} aria-label={`Set up ${stream.name}`}>
                      Set up
                    </Button>
                  </td>
                ) : null}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {editing ? (
        <StreamDialog
          key={editing.id}
          workspaceId={workspaceId}
          sourceId={sourceId}
          stream={editing}
          onClose={() => setEditing(null)}
          onSaved={(saved) => {
            setEditing(null);
            onChanged(saved);
          }}
        />
      ) : null}
    </>
  );
}
