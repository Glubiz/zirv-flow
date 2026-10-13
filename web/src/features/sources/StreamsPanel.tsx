import { useState } from "react";

import { Button, EmptyState, Pill } from "../../ui";
import { MappingDialog } from "./MappingDialog";
import { StreamDialog } from "./StreamDialog";
import { syncModeLabel, type Stream } from "./labels";

interface StreamsPanelProps {
  workspaceId: string;
  sourceId: string;
  streams: Stream[];
  canManage: boolean;
  onChanged: (stream: Stream) => void;
  /** Starts a run of one stream; without it the table offers no run. */
  onRun?: (stream: Stream) => void;
  busy?: boolean;
}

/** The streams of a source with their settings; admins set each one up in a dialog. */
export function StreamsPanel({ workspaceId, sourceId, streams, canManage, onChanged, onRun, busy }: StreamsPanelProps) {
  const [editing, setEditing] = useState<Stream | null>(null);
  const [mapping, setMapping] = useState<Stream | null>(null);

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
                <td>{syncModeLabel(stream.sync_mode)}</td>
                <td>{stream.cursor_field ?? "None"}</td>
                <td>{stream.primary_key.length > 0 ? stream.primary_key.join(", ") : "None"}</td>
                <td className="num">{stream.staleness_threshold_seconds ?? "None"}</td>
                {canManage ? (
                  <td className="num">
                    {onRun && stream.enabled && stream.mapping ? (
                      <Button onClick={() => onRun(stream)} disabled={busy === true} aria-label={`Run ${stream.name} now`}>
                        Run now
                      </Button>
                    ) : null}
                    <Button onClick={() => setMapping(stream)} aria-label={`Map ${stream.name}`}>
                      Map
                    </Button>
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
      {mapping ? (
        <MappingDialog
          key={mapping.id}
          workspaceId={workspaceId}
          sourceId={sourceId}
          stream={mapping}
          onClose={() => setMapping(null)}
          onSaved={(saved) => {
            setMapping(null);
            onChanged(saved);
          }}
        />
      ) : null}
    </>
  );
}
