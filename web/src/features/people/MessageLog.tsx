import { useEffect, useId, useRef, useState } from "react";
import { Link } from "react-router";

import { client, describeError, unwrap, useResource } from "../../api";
import { Button, EmptyState, formatDateTime, InlineError, Panel, ResourceView, shortId } from "../../ui";
import { traceHref } from "../traces";
import { explain, gateRows, resultOf, type GateRow, type Message, type MessageResult } from "./messageView";
import { useMessageLog, type MessageFilter, type ResultFilter } from "./useMessageLog";

interface MessageLogProps {
  workspaceId: string;
  personId: string;
}

const RESULTS: readonly { value: ResultFilter; label: string }[] = [
  { value: "", label: "All results" },
  { value: "sent", label: "Sent" },
  { value: "skipped", label: "Skipped" },
  { value: "deferred", label: "Deferred" },
  { value: "in_progress", label: "In progress" },
  { value: "failed", label: "Failed" },
];

function isResult(value: string): value is ResultFilter {
  return RESULTS.some((result) => result.value === value);
}

/** The messages sent to a person, newest first, with the checks each went through. */
export function MessageLog({ workspaceId, personId }: MessageLogProps) {
  const [filter, setFilter] = useState<MessageFilter>({ result: "", template: "" });
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set());
  const log = useMessageLog(workspaceId, personId, filter);
  const templates = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/templates", { params: { path: { ws: workspaceId }, query: { archived: true } } })),
    [workspaceId],
  );
  const resultId = useId();
  const templateId = useId();
  const rows = useRef<HTMLDivElement>(null);
  const footer = useRef<HTMLDivElement>(null);
  // How many messages were shown when "Load older" was pressed; focus moves once the page is in.
  const focusFrom = useRef<number | null>(null);
  const { loadingMore, resource } = log;
  useEffect(() => {
    const from = focusFrom.current;
    if (from === null || loadingMore) return;
    focusFrom.current = null;
    const first = resource.status === "ready" ? resource.data[from] : undefined;
    const target = first
      ? rows.current?.querySelector<HTMLElement>(`[data-message-toggle="${first.id}"]`)
      : footer.current?.querySelector<HTMLElement>("button");
    target?.focus();
  }, [loadingMore, resource]);

  function toggle(id: string) {
    setExpanded((current) => {
      const next = new Set(current);
      if (!next.delete(id)) next.add(id);
      return next;
    });
  }

  const templateOptions = templates.status === "ready" ? templates.data : [];
  const loaded = log.resource.status === "ready" ? log.resource.data : [];

  return (
    <Panel flush>
      <div className="panel-pad message-filters">
        <h2 className="panel-title">Messages</h2>
        <div className="field-inline">
          <label className="field-label" htmlFor={resultId}>
            Result
          </label>
          <select
            id={resultId}
            className="select"
            value={filter.result}
            onChange={(event) => {
              const value = event.target.value;
              if (isResult(value)) setFilter({ ...filter, result: value });
            }}
          >
            {RESULTS.map((result) => (
              <option key={result.value} value={result.value}>
                {result.label}
              </option>
            ))}
          </select>
        </div>
        <div className="field-inline">
          <label className="field-label" htmlFor={templateId}>
            Template
          </label>
          <select id={templateId} className="select" value={filter.template} onChange={(event) => setFilter({ ...filter, template: event.target.value })}>
            <option value="">All templates</option>
            {templateOptions.map((template) => (
              <option key={template.id} value={template.id}>
                {template.name}
              </option>
            ))}
          </select>
        </div>
        <span className="faint message-note">Newest first. Addresses are never shown here.</span>
      </div>
      <ResourceView
        resource={log.resource}
        loadingLabel="Loading the messages"
        isEmpty={(data) => data.length === 0}
        empty={
          <EmptyState title="No messages to this person yet.">
            {filter.result || filter.template ? "Nothing matches these filters." : "A message appears here when a flow sends or skips one."}
          </EmptyState>
        }
      >
        {(messages) => (
          <div className="table-scroll" ref={rows}>
            <table className="table message-table">
              <caption>Messages to this person, newest first, with the result of each</caption>
              <thead>
                <tr>
                  <th scope="col">Time</th>
                  <th scope="col">Template</th>
                  <th scope="col">Flow and step</th>
                  <th scope="col">Destination</th>
                  <th scope="col">Result</th>
                  <th scope="col">
                    <span className="visually-hidden">Details</span>
                  </th>
                </tr>
              </thead>
              <tbody>
                {messages.map((message) => (
                  <MessageRows
                    key={message.id}
                    workspaceId={workspaceId}
                    message={message}
                    open={expanded.has(message.id)}
                    onToggle={() => toggle(message.id)}
                  />
                ))}
              </tbody>
            </table>
          </div>
        )}
      </ResourceView>
      {log.resource.status === "ready" && log.nextBefore !== null ? (
        <div className="panel-footer" ref={footer}>
          {log.moreError ? <InlineError {...describeError(log.moreError)} /> : null}
          <Button
            onClick={() => {
              focusFrom.current = loaded.length;
              log.loadOlder();
            }}
            disabled={log.loadingMore}
          >
            {log.loadingMore ? "Loading" : "Load older"}
          </Button>
          {loaded.length > 0 ? <span className="faint">Showing messages since {formatDateTime(loaded[loaded.length - 1]?.created_at ?? "")}</span> : null}
        </div>
      ) : null}
    </Panel>
  );
}

function MessageRows({ workspaceId, message, open, onToggle }: { workspaceId: string; message: Message; open: boolean; onToggle: () => void }) {
  const detailId = `message-${message.id}-checks`;
  const name = message.template.name ?? `template ${shortId(message.template.id)}`;
  const result = resultOf(message);
  return (
    <>
      <tr className={open ? "message-open" : undefined}>
        <td className="message-time">
          <time dateTime={message.created_at}>{formatDateTime(message.created_at)}</time>
        </td>
        <td>
          <div className="strong">{name}</div>
          <div className="faint">{templateLine(message)}</div>
        </td>
        <td>
          {message.flow ? (
            <Link to={`/w/${workspaceId}/flows/${message.flow.id}`}>{message.flow.name ?? `Flow ${shortId(message.flow.id)}`}</Link>
          ) : (
            <span className="faint">—</span>
          )}
          <div className="faint">{message.node.name ?? <span className="mono">{message.node.id}</span>}</div>
        </td>
        <td>{message.destination.name ?? <span className="faint">{shortId(message.destination.id)}</span>}</td>
        <td>
          <ResultText result={result} />
        </td>
        <td>
          <button
            type="button"
            className="icon-btn"
            aria-expanded={open}
            aria-controls={detailId}
            data-message-toggle={message.id}
            aria-label={`${open ? "Hide" : "Show"} the checks for ${name}`}
            onClick={onToggle}
          >
            <svg aria-hidden="true" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round">
              <path d={open ? "M4 10l4-4 4 4" : "M4 6l4 4 4-4"} />
            </svg>
          </button>
        </td>
      </tr>
      <tr id={detailId} className="message-open" hidden={!open}>
        <td colSpan={6} className="message-detail-cell">
          {open ? <Checks workspaceId={workspaceId} message={message} result={result} /> : null}
        </td>
      </tr>
    </>
  );
}

function templateLine(message: Message): string {
  const parts = [message.template.version ? `v${message.template.version}` : null, message.template.category ? capitalize(message.template.category) : null];
  return parts.filter(Boolean).join(" · ");
}

function capitalize(text: string): string {
  return text.charAt(0).toUpperCase() + text.slice(1);
}

const ICONS: Record<MessageResult["kind"], string> = {
  sent: "M5.5 8.2l1.8 1.8 3.3-3.6",
  skipped: "M3.6 12.4l8.8-8.8",
  deferred: "M8 4.5V8l2.5 1.5",
  failed: "M5.5 5.5l5 5M10.5 5.5l-5 5",
  waiting: "M8 4.5V8l2.5 1.5",
  sending: "M5 8h6",
};

const TONES: Record<MessageResult["kind"], string> = {
  sent: "st-ok",
  skipped: "st-skip",
  deferred: "st-defer",
  failed: "st-fail",
  waiting: "st-defer",
  sending: "st-defer",
};

function ResultText({ result }: { result: MessageResult }) {
  return (
    <>
      <span className={`st ${TONES[result.kind]}`}>
        <svg aria-hidden="true" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round">
          <circle cx="8" cy="8" r="6.25" />
          <path d={ICONS[result.kind]} />
        </svg>
        <span>
          {result.headline}
          {result.code ? (
            <>
              : <span className="mono">{result.code}</span>
            </>
          ) : null}
          {result.rule ? <>, rule {result.rule}</> : null}
        </span>
      </span>
      {result.note ? <div className="faint">{result.note}</div> : null}
    </>
  );
}

const GATE_WORDS: Record<GateRow["state"], string> = {
  passed: "Passed",
  stopped: "Stopped here",
  held: "Held here",
  not_reached: "Not reached",
};

const GATE_TONES: Record<GateRow["state"], string> = {
  passed: "st st-ok",
  stopped: "st st-skip",
  held: "st st-defer",
  not_reached: "faint",
};

function Checks({ workspaceId, message, result }: { workspaceId: string; message: Message; result: MessageResult }) {
  const rows = gateRows(message);
  return (
    <div className="message-detail">
      <ol className="message-gates" aria-label="Send checks, in the order they run">
        {rows.map((row, index) => (
          <li key={row.code} className={`message-gate message-gate-${row.state}`}>
            <span className="faint">{index + 1}</span>
            <span className="strong">{row.label}</span>
            <span className={GATE_TONES[row.state]}>
              {GATE_WORDS[row.state]}
              {row.reason ? (
                <>
                  {" "}
                  <span className="mono">{row.reason}</span>
                </>
              ) : null}
            </span>
          </li>
        ))}
      </ol>
      <div className="message-cards">
        <div className="message-card">
          <p className="strong">{cardTitle(result)}</p>
          <p>{explain(message)}</p>
        </div>
        <div className="message-card">
          {message.trace_id ? <Link to={traceHref(workspaceId, message.trace_id)}>Open in Traces</Link> : <span className="faint">No trace was recorded for this message.</span>}
          {message.consent_policy_id && message.consent_rule_id ? (
            <span className="faint">
              Consent policy <span className="mono">{shortId(message.consent_policy_id)}</span>, rule <span className="mono">{shortId(message.consent_rule_id)}</span>
            </span>
          ) : null}
          <span className="faint">
            Message <span className="mono">{shortId(message.id)}</span>
            {message.trace_id ? (
              <>
                {" "}
                · trace <span className="mono">{shortId(message.trace_id)}</span>
              </>
            ) : null}
          </span>
        </div>
      </div>
    </div>
  );
}

function cardTitle(result: MessageResult): string {
  return `${result.headline}${result.code ? `: ${result.code}` : ""}`;
}
