import { useEffect, useRef, useState, type SyntheticEvent } from "react";
import { Link, useParams } from "react-router";

import type { components } from "../../api";
import { Button, EmptyState, PageHeader, Panel, Pill, SelectField, TextField, formatDateTime, type Tone } from "../../ui";
import { useWorkspaceAccess } from "../members";
import { EventsNav } from "./EventsNav";
import { PropertyFacts } from "./PropertyFacts";
import { OUTCOMES, outcomeLabel, reasonLabel, rejectReasonLabel } from "./labels";

type Item = components["schemas"]["DebuggerItemBody"];

type StreamStatus = "connecting" | "live" | "reconnecting" | "closed";

/** The most rows the page keeps; older ones fall off the end. */
const KEEP = 300;

const TONES: Record<string, Tone> = {
  accepted: "neutral",
  duplicate: "neutral",
  flagged: "attention",
  unplanned: "attention",
  rejected: "error",
};

interface Filters {
  source: string;
  name: string;
  outcome: string;
}

const NO_FILTERS: Filters = { source: "", name: "", outcome: "" };

export function DebuggerPage() {
  const { ws = "" } = useParams();
  const access = useWorkspaceAccess(ws);
  const canReveal = access.status === "ready" && access.data.canManage;

  return (
    <div className="page">
      <PageHeader
        title="Event debugger"
        lede="Events as they are decided, newest first. Nothing is kept for the page: it shows what arrives while it is open."
      />
      <EventsNav workspaceId={ws} />
      <Debugger key={ws} workspaceId={ws} canReveal={canReveal} />
    </div>
  );
}

function streamUrl(workspaceId: string, filters: Filters, reveal: boolean): string {
  const query = new URLSearchParams();
  if (filters.source !== "") query.set("source", filters.source);
  if (filters.name !== "") query.set("name", filters.name);
  if (filters.outcome !== "") query.set("outcome", filters.outcome);
  if (reveal) query.set("reveal", "true");
  const text = query.toString();
  return `/api/v1/workspaces/${workspaceId}/debugger/stream${text === "" ? "" : `?${text}`}`;
}

function Debugger({ workspaceId, canReveal }: { workspaceId: string; canReveal: boolean }) {
  const [draft, setDraft] = useState<Filters>(NO_FILTERS);
  const [applied, setApplied] = useState<Filters>(NO_FILTERS);
  const [reveal, setReveal] = useState(false);
  const [attempt, setAttempt] = useState(0);

  function apply(event: SyntheticEvent) {
    event.preventDefault();
    setApplied({ source: draft.source.trim(), name: draft.name.trim(), outcome: draft.outcome });
  }

  const url = streamUrl(workspaceId, applied, reveal && canReveal);

  return (
    <>
      <form className="inline-form" onSubmit={apply} role="search" aria-label="Filter the live feed">
        <TextField
          label="Source"
          value={draft.source}
          onChange={(event) => setDraft({ ...draft, source: event.target.value })}
          autoComplete="off"
        />
        <TextField
          label="Event name"
          value={draft.name}
          onChange={(event) => setDraft({ ...draft, name: event.target.value })}
          autoComplete="off"
        />
        <SelectField label="Outcome" value={draft.outcome} onChange={(event) => setDraft({ ...draft, outcome: event.target.value })}>
          <option value="">Any outcome</option>
          {OUTCOMES.map((outcome) => (
            <option key={outcome.value} value={outcome.value}>
              {outcome.label}
            </option>
          ))}
        </SelectField>
        <Button type="submit">Apply filters</Button>
      </form>
      {canReveal ? (
        <label className="check">
          <input type="checkbox" checked={reveal} onChange={(event) => setReveal(event.target.checked)} />
          <span>
            Reveal PII
            <span className="field-hint"> Shows stored values. Anything the workspace redacts at ingest stays redacted.</span>
          </span>
        </label>
      ) : null}
      <LiveFeed key={`${url}#${attempt}`} workspaceId={workspaceId} url={url} onReconnect={() => setAttempt((value) => value + 1)} />
    </>
  );
}

function statusText(status: StreamStatus): string {
  switch (status) {
    case "connecting":
      return "Connecting";
    case "live":
      return "Live";
    case "reconnecting":
      return "Connection lost, reconnecting";
    case "closed":
      return "Disconnected";
  }
}

function parse(event: Event): unknown {
  try {
    return JSON.parse((event as MessageEvent<string>).data) as unknown;
  } catch {
    return null;
  }
}

function isItem(value: unknown): value is Item {
  return typeof value === "object" && value !== null && "seq" in value && "outcome" in value;
}

function LiveFeed({ workspaceId, url, onReconnect }: { workspaceId: string; url: string; onReconnect: () => void }) {
  const [items, setItems] = useState<Item[]>([]);
  const [held, setHeld] = useState<Item[]>([]);
  const [paused, setPaused] = useState(false);
  const [status, setStatus] = useState<StreamStatus>("connecting");
  const [skipped, setSkipped] = useState(0);
  const [reset, setReset] = useState(false);
  const [degraded, setDegraded] = useState(false);
  const pausedRef = useRef(false);

  useEffect(() => {
    pausedRef.current = paused;
  }, [paused]);

  useEffect(() => {
    const source = new EventSource(url);
    source.onopen = () => setStatus("live");
    source.onerror = () => setStatus(source.readyState === 2 ? "closed" : "reconnecting");
    source.addEventListener("item", (event) => {
      const item = parse(event);
      if (!isItem(item)) return;
      // A resume can repeat rows the page already has; the seq says which.
      if (pausedRef.current) {
        setHeld((current) => (current.some((other) => other.seq === item.seq) ? current : [item, ...current].slice(0, KEEP)));
      } else {
        setItems((current) => (current.some((other) => other.seq === item.seq) ? current : [item, ...current].slice(0, KEEP)));
      }
    });
    source.addEventListener("gap", (event) => {
      const gap = parse(event);
      const count = typeof gap === "object" && gap !== null && "skipped" in gap && typeof gap.skipped === "number" ? gap.skipped : 0;
      setSkipped((total) => total + count);
      if (typeof gap === "object" && gap !== null && "reset" in gap && gap.reset === true) {
        setReset(true);
      }
    });
    source.addEventListener("status", (event) => {
      const status = parse(event);
      if (typeof status === "object" && status !== null && "degraded" in status) {
        setDegraded(status.degraded === true);
      }
    });
    return () => {
      source.close();
    };
  }, [url]);

  function resume() {
    setItems((current) => [...held, ...current].slice(0, KEEP));
    setHeld([]);
    setPaused(false);
  }

  return (
    <>
      <div className="row" role="group" aria-label="Feed controls">
        <p role="status" className="strong">
          <Pill tone={status === "live" ? "neutral" : "attention"}>{statusText(status)}</Pill>
        </p>
        {paused ? (
          <Button onClick={resume}>Resume{held.length > 0 ? ` (${held.length} held)` : ""}</Button>
        ) : (
          <Button onClick={() => setPaused(true)}>Pause</Button>
        )}
        {status === "closed" ? <Button onClick={onReconnect}>Reconnect</Button> : null}
        {items.length > 0 ? (
          <Button
            onClick={() => {
              setItems([]);
            }}
          >
            Clear
          </Button>
        ) : null}
      </div>
      {degraded ? (
        <p className="notice" role="status">
          Live updates are delayed: the server is checking for new events every few seconds instead of being told about them.
        </p>
      ) : null}
      {reset ? (
        <p className="notice" role="status">
          The feed was reset on the server, so events from before this moment may be missing.
          <Button
            onClick={() => {
              setReset(false);
            }}
          >
            Dismiss
          </Button>
        </p>
      ) : null}
      {skipped > 0 ? (
        <p className="notice" role="status">
          The page could not keep up, so about {skipped} {skipped === 1 ? "event was" : "events were"} skipped.
          <Button
            onClick={() => {
              setSkipped(0);
            }}
          >
            Dismiss
          </Button>
        </p>
      ) : null}
      <Panel flush>
        {items.length === 0 ? (
          <EmptyState title={paused ? "Paused" : "Waiting for events"}>
            {paused
              ? "New events are held until you resume."
              : "Send an event with an ingest key and it appears here as soon as it is decided."}
          </EmptyState>
        ) : (
          <div className="table-scroll">
            <table className="table">
              <caption>Live events</caption>
              <thead>
                <tr>
                  <th scope="col">Time</th>
                  <th scope="col">Outcome</th>
                  <th scope="col">Event</th>
                  <th scope="col">Source</th>
                  <th scope="col">Details</th>
                </tr>
              </thead>
              <tbody>
                {items.map((item) => (
                  <ItemRow key={item.seq} workspaceId={workspaceId} item={item} />
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Panel>
    </>
  );
}

function ItemRow({ workspaceId, item }: { workspaceId: string; item: Item }) {
  return (
    <tr>
      <td>{formatDateTime(item.at)}</td>
      <td>
        <Pill tone={TONES[item.outcome] ?? "neutral"}>{outcomeLabel(item.outcome)}</Pill>
      </td>
      <td className="strong">{item.name ?? <span className="faint">invalid name</span>}</td>
      <td>{item.source}</td>
      <td>
        <details className="sample">
          <summary>
            Show details <span className="visually-hidden">of the {item.name ?? "rejected"} event</span>
          </summary>
          {item.reason ? <p>{rejectReasonLabel(item.reason)}</p> : null}
          {item.reasons.length > 0 ? (
            <ul className="records list-plain">
              {item.reasons.map((reason, index) => (
                <li key={index}>
                  <span className="mono">{reason.property}</span> <span className="faint">{reasonLabel(reason.reason)}</span>
                </li>
              ))}
            </ul>
          ) : null}
          <PropertyFacts properties={item.properties} masked={item.masked} />
          <p>
            <Link to={`/w/${workspaceId}/traces/${item.trace_id}`}>Open the trace</Link>
          </p>
        </details>
      </td>
    </tr>
  );
}
