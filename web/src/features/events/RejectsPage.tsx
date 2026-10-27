import { useState, type SyntheticEvent } from "react";
import { useParams } from "react-router";

import { client, unwrap, useResource, type components } from "../../api";
import { Button, EmptyState, PageHeader, Panel, Pill, ResourceView, TextField, formatDateTime } from "../../ui";
import { useWorkspaceAccess } from "../members";
import { EventsNav } from "./EventsNav";
import { PropertyFacts } from "./PropertyFacts";
import { ReplayDialog, type ReplayTarget } from "./ReplayDialog";
import { reasonLabel, rejectReasonLabel } from "./labels";

type Reject = components["schemas"]["IngestRejectBody"];

export function RejectsPage() {
  const { ws = "" } = useParams();
  const access = useWorkspaceAccess(ws);
  const canReplay = access.status === "ready" && access.data.canAdmin;

  return (
    <div className="page">
      <PageHeader
        title="Rejects"
        lede="Events that were refused, newest first. Values are masked: only properties your schema marks as not personal data are shown."
      />
      <EventsNav workspaceId={ws} />
      <RejectList key={ws} workspaceId={ws} canReplay={canReplay} />
    </div>
  );
}

function RejectList({ workspaceId, canReplay }: { workspaceId: string; canReplay: boolean }) {
  const [name, setName] = useState("");
  const [reason, setReason] = useState("");
  const [applied, setApplied] = useState({ name: "", reason: "" });
  const [before, setBefore] = useState<string | null>(null);
  const [selected, setSelected] = useState<ReadonlySet<string>>(new Set());
  const [replay, setReplay] = useState<ReplayTarget | null>(null);

  const rejects = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/ingest-rejects", {
          params: {
            path: { ws: workspaceId },
            query: {
              ...(applied.name !== "" ? { name: applied.name } : {}),
              ...(applied.reason !== "" ? { reason: applied.reason } : {}),
              ...(before !== null ? { before } : {}),
            },
          },
        }),
      ),
    [workspaceId, applied.name, applied.reason, before],
  );

  function toggle(id: string, on: boolean) {
    const next = new Set(selected);
    if (on) next.add(id);
    else next.delete(id);
    setSelected(next);
  }

  function filter(event: SyntheticEvent) {
    event.preventDefault();
    setBefore(null);
    setSelected(new Set());
    setApplied({ name: name.trim(), reason: reason.trim() });
  }

  const filtered = applied.name !== "" || applied.reason !== "";

  return (
    <>
      <form className="inline-form" onSubmit={filter} role="search" aria-label="Filter rejects">
        <TextField label="Event name" value={name} onChange={(event) => setName(event.target.value)} autoComplete="off" />
        <TextField
          label="Reason code"
          mono
          hint="For example validation_failed"
          value={reason}
          onChange={(event) => setReason(event.target.value)}
          autoComplete="off"
        />
        <Button type="submit">Filter</Button>
      </form>
      {canReplay ? (
        <div className="row" role="group" aria-label="Replay">
          <Button disabled={selected.size === 0} onClick={() => setReplay({ kind: "ids", ids: [...selected] })}>
            Replay selected{selected.size > 0 ? ` (${selected.size})` : ""}
          </Button>
          <Button onClick={() => setReplay({ kind: "filter", name: applied.name, reason: applied.reason })}>
            {filtered ? "Replay all that match the filter" : "Replay all open rejects"}
          </Button>
        </div>
      ) : null}
      {replay ? (
        <ReplayDialog
          workspaceId={workspaceId}
          target={replay}
          onClose={() => setReplay(null)}
          onQueued={() => {
            setSelected(new Set());
            rejects.reload();
          }}
        />
      ) : null}
      <Panel flush>
        <ResourceView
          resource={rejects}
          loadingLabel="Loading rejects"
          isEmpty={(data) => data.rejects.length === 0}
          empty={
            <EmptyState title={filtered ? "No rejects match" : "No rejects"}>
              {filtered
                ? "Try a different event name or reason."
                : "Nothing has been refused. Events that fail a blocking schema, or have no schema when the workspace rejects unplanned events, appear here."}
            </EmptyState>
          }
        >
          {(data) => (
            <>
              <div className="table-scroll">
                <table className="table">
                  <caption>Rejected events</caption>
                  <thead>
                    <tr>
                      {canReplay ? (
                        <th scope="col">
                          <span className="visually-hidden">Select</span>
                        </th>
                      ) : null}
                      <th scope="col">Received</th>
                      <th scope="col">Event</th>
                      <th scope="col">Source</th>
                      <th scope="col">Reason</th>
                      <th scope="col">Sample</th>
                    </tr>
                  </thead>
                  <tbody>
                    {data.rejects.map((reject) => (
                      <RejectRow
                        key={reject.id}
                        reject={reject}
                        selectable={canReplay}
                        selected={selected.has(reject.id)}
                        onSelect={(on) => toggle(reject.id, on)}
                      />
                    ))}
                  </tbody>
                </table>
              </div>
              <div className="panel-pad row">
                {before !== null ? <Button onClick={() => setBefore(null)}>Back to the newest</Button> : null}
                {data.next_before ? <Button onClick={() => setBefore(data.next_before ?? null)}>Older rejects</Button> : null}
              </div>
            </>
          )}
        </ResourceView>
      </Panel>
    </>
  );
}

interface RejectRowProps {
  reject: Reject;
  selectable: boolean;
  selected: boolean;
  onSelect: (on: boolean) => void;
}

function RejectRow({ reject, selectable, selected, onSelect }: RejectRowProps) {
  const sample = reject.sample as { properties?: Record<string, unknown>; masked?: string[] };
  return (
    <tr>
      {selectable ? (
        <td>
          {reject.replayed_at ? (
            <span className="faint">Replayed</span>
          ) : (
            <input
              type="checkbox"
              checked={selected}
              onChange={(event) => onSelect(event.target.checked)}
              aria-label={`Select the ${reject.name ?? "rejected"} reject received ${formatDateTime(reject.received_at)}`}
            />
          )}
        </td>
      ) : null}
      <td>{formatDateTime(reject.received_at)}</td>
      <td className="strong">{reject.name ?? <span className="faint">invalid name</span>}</td>
      <td>{reject.source}</td>
      <td>
        <Pill tone="attention">{rejectReasonLabel(reject.reason)}</Pill>
        {reject.reasons.length > 0 ? (
          <ul className="records list-plain">
            {reject.reasons.map((item, index) => (
              <li key={index}>
                <span className="mono">{item.property}</span> <span className="faint">{reasonLabel(item.reason)}</span>
              </li>
            ))}
          </ul>
        ) : null}
      </td>
      <td>
        <details className="sample">
          <summary>
            Show sample <span className="visually-hidden">of the {reject.name ?? "rejected"} event</span>
          </summary>
          <PropertyFacts properties={sample.properties ?? {}} masked={sample.masked ?? []} />
        </details>
      </td>
    </tr>
  );
}
