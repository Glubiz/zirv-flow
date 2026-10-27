import { Link } from "react-router";

import { CopyButton, Pill, Sheet } from "../../ui";
import { formatDateTime, shortId } from "../../ui/format";
import { traceHref } from "../traces";
import { actionLabel } from "./areas";
import { changesOf, objectHref, type AuditEntry } from "./model";
import { actorKindLabel } from "./AuditTable";

interface Props {
  workspaceId: string;
  entry: AuditEntry;
  onClose: () => void;
}

/** One entry in full: who, what, when, the request and the changes, with the trace one click away. */
export function AuditEntrySheet({ workspaceId, entry, onClose }: Props) {
  const changes = changesOf(entry.summary);
  const href = objectHref(workspaceId, entry);
  const object = entry.object_id ? shortId(entry.object_id) : null;
  return (
    <Sheet
      title={actionLabel(entry.action)}
      eyebrow="Audit entry"
      closeLabel="Close entry"
      onClose={onClose}
      modal
    >
      <p className="mono audit-code">{entry.action}</p>
      <dl className="audit-facts">
        <dt>Time</dt>
        <dd>{formatDateTime(entry.occurred_at)}</dd>
        <dt>Actor</dt>
        <dd>
          <Pill>{actorKindLabel(entry.actor_kind)}</Pill>
          {entry.actor_id ? (
            <span className="mono" title={entry.actor_id}>
              {" "}
              {shortId(entry.actor_id)}
            </span>
          ) : null}
        </dd>
        <dt>Object</dt>
        <dd>
          {object === null ? (
            "None"
          ) : (
            <>
              {entry.object_kind && entry.object_kind !== "person" ? `${entry.object_kind} ` : null}
              {href ? (
                <Link className="mono" to={href} title={entry.object_id ?? undefined}>
                  {object}
                </Link>
              ) : (
                <span className="mono" title={entry.object_id ?? undefined}>
                  {object}
                </span>
              )}
            </>
          )}
        </dd>
        <dt>Request</dt>
        <dd>
          {entry.request_id ? (
            <>
              <span className="mono audit-request">{entry.request_id}</span>{" "}
              <CopyButton value={entry.request_id} label="Copy request id" />
            </>
          ) : (
            "None"
          )}
        </dd>
      </dl>
      <section aria-labelledby="audit-changes">
        <h3 className="audit-changes-title" id="audit-changes">
          Changes
        </h3>
        {changes.length === 0 ? (
          <p className="faint">No details recorded.</p>
        ) : (
          <dl className="audit-facts">
            {changes.map((change) => (
              <div className="audit-change" key={change.key}>
                <dt>{change.label}</dt>
                <dd>{change.value}</dd>
              </div>
            ))}
          </dl>
        )}
      </section>
      {entry.trace_id ? (
        <div className="audit-sheet-foot">
          <Link className="btn btn-primary" to={traceHref(workspaceId, entry.trace_id)}>
            Open trace
          </Link>
        </div>
      ) : null}
    </Sheet>
  );
}
