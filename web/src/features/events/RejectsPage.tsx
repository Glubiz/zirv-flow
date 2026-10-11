import { useState, type SyntheticEvent } from "react";
import { useParams } from "react-router";

import { client, unwrap, useResource, type components } from "../../api";
import { Button, EmptyState, PageHeader, Panel, Pill, ResourceView, TextField, formatDateTime } from "../../ui";
import { EventsNav } from "./EventsNav";
import { reasonLabel, rejectReasonLabel } from "./labels";

type Reject = components["schemas"]["IngestRejectBody"];

export function RejectsPage() {
  const { ws = "" } = useParams();

  return (
    <div className="page">
      <PageHeader
        title="Rejects"
        lede="Events that were refused, newest first. Values are masked: only properties your schema marks as not personal data are shown."
      />
      <EventsNav workspaceId={ws} />
      <RejectList key={ws} workspaceId={ws} />
    </div>
  );
}

function RejectList({ workspaceId }: { workspaceId: string }) {
  const [name, setName] = useState("");
  const [reason, setReason] = useState("");
  const [applied, setApplied] = useState({ name: "", reason: "" });
  const [before, setBefore] = useState<string | null>(null);

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

  function filter(event: SyntheticEvent) {
    event.preventDefault();
    setBefore(null);
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
                      <th scope="col">Received</th>
                      <th scope="col">Event</th>
                      <th scope="col">Source</th>
                      <th scope="col">Reason</th>
                      <th scope="col">Sample</th>
                    </tr>
                  </thead>
                  <tbody>
                    {data.rejects.map((reject) => (
                      <RejectRow key={reject.id} reject={reject} />
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

function RejectRow({ reject }: { reject: Reject }) {
  const sample = reject.sample as { properties?: Record<string, unknown>; masked?: string[] };
  const properties = Object.entries(sample.properties ?? {});
  const masked = new Set(sample.masked ?? []);
  return (
    <tr>
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
          {properties.length === 0 ? (
            <p className="faint">No properties.</p>
          ) : (
            <dl className="facts">
              {properties.map(([key, value]) => (
                <div key={key} className="fact">
                  <dt className="mono">{key}</dt>
                  <dd>{masked.has(key) ? <span className="faint">masked</span> : <span className="mono">{JSON.stringify(value)}</span>}</dd>
                </div>
              ))}
            </dl>
          )}
        </details>
      </td>
    </tr>
  );
}
