import { Pill } from "../../ui";
import { formatDate, formatTime, shortId } from "../../ui/format";
import { actionLabel } from "./areas";
import type { AuditEntry } from "./model";

export function actorKindLabel(kind: string): string {
  switch (kind) {
    case "user":
      return "User";
    case "api_key":
      return "API key";
    case "system":
      return "System";
    case "anonymous":
      return "Anonymous";
    default:
      return kind;
  }
}

function Actor({ entry }: { entry: AuditEntry }) {
  const id = entry.actor_id ? (
    <span className="mono" title={entry.actor_id}>
      {shortId(entry.actor_id)}
    </span>
  ) : null;
  if (entry.actor_kind === "api_key") {
    return (
      <span className="audit-actor">
        <Pill>API key</Pill> {id}
      </span>
    );
  }
  return (
    <span className="audit-actor">
      {actorKindLabel(entry.actor_kind)} {id}
    </span>
  );
}

interface Props {
  entries: AuditEntry[];
  selectedId: number | null;
  onSelect: (entry: AuditEntry) => void;
}

export function AuditTable({ entries, selectedId, onSelect }: Props) {
  return (
    <div className="table-scroll">
      <table className="table audit-table">
        <caption>Audit log, newest first</caption>
        <thead>
          <tr>
            <th scope="col">Time</th>
            <th scope="col">Actor</th>
            <th scope="col">Action</th>
            <th scope="col">Object</th>
            <th scope="col">Request</th>
          </tr>
        </thead>
        <tbody>
          {entries.map((entry) => (
            <tr key={entry.id} className={entry.id === selectedId ? "audit-row audit-row-selected" : "audit-row"}>
              <td>
                <button
                  type="button"
                  className="audit-open"
                  aria-label={`Open entry ${entry.action}`}
                  aria-pressed={entry.id === selectedId}
                  onClick={() => onSelect(entry)}
                >
                  {formatTime(entry.occurred_at)}
                </button>
                <span className="faint audit-date">{formatDate(entry.occurred_at)}</span>
              </td>
              <td>
                <Actor entry={entry} />
              </td>
              <td>
                <span className="audit-label">{actionLabel(entry.action)}</span>
                <span className="mono faint audit-code">{entry.action}</span>
              </td>
              <td>
                {entry.object_id ? (
                  <>
                    {entry.object_kind && entry.object_kind !== "person" ? `${entry.object_kind} ` : null}
                    <span className="mono" title={entry.object_id}>
                      {shortId(entry.object_id)}
                    </span>
                  </>
                ) : (
                  <span className="faint">None</span>
                )}
              </td>
              <td>
                {entry.request_id ? (
                  <span className="mono" title={entry.request_id}>
                    {shortId(entry.request_id)}
                  </span>
                ) : (
                  <span className="faint">None</span>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
