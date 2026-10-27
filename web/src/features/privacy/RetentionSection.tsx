import { useState } from "react";
import { Link } from "react-router";

import { client, unwrap, useResource, type components } from "../../api";
import { Button, formatDateTime, MenuButton, Panel, plural, ResourceView } from "../../ui";
import { formatWindow } from "./retentionUnits";
import { ShortenWindowDialog, type EditableWindow } from "./ShortenWindowDialog";

type Window = components["schemas"]["WindowBody"];

const LABELS: Record<string, string> = {
  events: "Events",
  messages: "Messages",
  decision_log: "Decision log",
  audit: "Audit log",
  ingest_rejects: "Ingest rejects",
  import_files: "Import files",
  job_history: "Job history",
  assistant_transcripts: "Assistant transcripts",
};

function labelOf(window: Window): string {
  return LABELS[window.key] ?? window.key;
}

function LockIcon() {
  return (
    <svg width="12" height="12" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.6" aria-hidden="true">
      <rect x="3" y="7" width="10" height="7" rx="1.5" />
      <path d="M5 7V5a3 3 0 0 1 6 0v2" />
    </svg>
  );
}

function PurgeMenu({ window }: { window: Window }) {
  const run = window.last_run;
  return (
    <MenuButton
      label={`Last purge for ${labelOf(window)}`}
      buttonClassName="icon-btn"
      buttonContent={<span aria-hidden="true">⋯</span>}
      heading="Last purge"
      entries={[
        {
          key: "run",
          label: run ? formatDateTime(run.ran_at) : "Not run yet",
          reason: run ? `${plural(run.deleted, "row", "rows")} removed` : undefined,
          disabled: true,
          onSelect: () => undefined,
        },
      ]}
    />
  );
}

function Change({
  workspaceId,
  window,
  canAdmin,
  onShorten,
}: {
  workspaceId: string;
  window: Window;
  canAdmin: boolean;
  onShorten: (window: Window) => void;
}) {
  if (window.key === "events") {
    return <Link to={`/w/${workspaceId}/events`}>Per-event overrides</Link>;
  }
  if (window.key === "assistant_transcripts") {
    return <Link to={`/w/${workspaceId}/ai`}>Assistant settings</Link>;
  }
  if (window.editable) {
    return canAdmin ? (
      <Button aria-label={`Shorten ${labelOf(window)}`} onClick={() => onShorten(window)}>
        Shorten
      </Button>
    ) : null;
  }
  if (window.source === "deployment") {
    return (
      <span className="pill">
        <LockIcon />
        Deployment
      </span>
    );
  }
  return null;
}

function toEditable(window: Window): EditableWindow | null {
  if (window.value == null || (window.key !== "messages" && window.key !== "decision_log")) {
    return null;
  }
  return { key: window.key, label: labelOf(window), value: window.value, min: window.min ?? null, max: window.max ?? null };
}

/** Every retention window in force. A workspace admin can shorten the two that the API marks editable. */
export function RetentionSection({ workspaceId, canAdmin }: { workspaceId: string; canAdmin: boolean }) {
  const retention = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/retention", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const [editing, setEditing] = useState<EditableWindow | null>(null);

  return (
    <Panel title="Retention" flush>
      <ResourceView resource={retention} loadingLabel="Loading retention">
        {(data) => (
          <div className="table-scroll">
            <table className="table">
              <caption>Retention windows</caption>
              <thead>
                <tr>
                  <th scope="col">Window</th>
                  <th scope="col">Keeps</th>
                  <th scope="col">Change</th>
                  <th scope="col">
                    <span className="visually-hidden">Last purge</span>
                  </th>
                </tr>
              </thead>
              <tbody>
                {data.windows.map((window) => (
                  <tr key={window.key}>
                    <td className="strong">
                      {labelOf(window)}
                    </td>
                    <td>{window.value == null ? "Off" : formatWindow(window.value)}</td>
                    <td>
                      <Change workspaceId={workspaceId} window={window} canAdmin={canAdmin} onShorten={(w) => setEditing(toEditable(w))} />
                    </td>
                    <td>
                      <PurgeMenu window={window} />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </ResourceView>
      {editing ? (
        <ShortenWindowDialog
          workspaceId={workspaceId}
          window={editing}
          onClose={() => setEditing(null)}
          onSaved={() => {
            setEditing(null);
            retention.reload();
          }}
        />
      ) : null}
    </Panel>
  );
}
