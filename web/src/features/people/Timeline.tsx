import { useState } from "react";
import { Link } from "react-router";

import { describeError } from "../../api";
import { Button, EmptyState, formatDateTime, HiddenValue, InlineError, Panel, Pill, ResourceView } from "../../ui";
import { actionLabel, outcomeLabel, reasonText, validationLabel } from "./timelineLabels";
import { useTimeline, type TimelineEntry, type TimelineKinds } from "./useTimeline";

interface TimelineProps {
  workspaceId: string;
  personId: string;
}

/** What the system saw and did about a person, newest first, each entry linking to its trace. */
export function Timeline({ workspaceId, personId }: TimelineProps) {
  const [kinds, setKinds] = useState<TimelineKinds>({ events: true, decisions: true });
  const timeline = useTimeline(workspaceId, personId, kinds);

  return (
    <Panel title="Timeline" flush>
      <div className="panel-pad">
        <fieldset className="row timeline-filter">
          <legend className="field-label">Show</legend>
          <label className="row">
            <input
              type="checkbox"
              checked={kinds.events}
              disabled={kinds.events && !kinds.decisions}
              onChange={(event) => setKinds({ ...kinds, events: event.target.checked })}
            />
            Events
          </label>
          <label className="row">
            <input
              type="checkbox"
              checked={kinds.decisions}
              disabled={kinds.decisions && !kinds.events}
              onChange={(event) => setKinds({ ...kinds, decisions: event.target.checked })}
            />
            Decisions
          </label>
        </fieldset>
      </div>
      <ResourceView
        resource={timeline.resource}
        loadingLabel="Loading the timeline"
        isEmpty={(data) => data.length === 0}
        empty={<EmptyState title="Nothing has happened yet">Events and changes about this person appear here.</EmptyState>}
      >
        {(entries) => (
          <ol className="list-plain timeline" aria-label="Timeline, newest first">
            {entries.map((entry) => (
              <TimelineItem key={`${entry.type}-${entry.id}`} workspaceId={workspaceId} entry={entry} />
            ))}
          </ol>
        )}
      </ResourceView>
      {timeline.resource.status === "ready" && timeline.nextBefore !== null ? (
        <div className="panel-footer">
          {timeline.moreError ? <InlineError {...describeError(timeline.moreError)} /> : null}
          <Button onClick={timeline.loadOlder} disabled={timeline.loadingMore}>
            {timeline.loadingMore ? "Loading" : "Load older"}
          </Button>
        </div>
      ) : null}
    </Panel>
  );
}

function TimelineItem({ workspaceId, entry }: { workspaceId: string; entry: TimelineEntry }) {
  const title = entry.type === "event" ? entry.name : actionLabel(entry.action);
  return (
    <li className="timeline-item">
      <time className="timeline-time faint" dateTime={entry.at}>
        {formatDateTime(entry.at)}
      </time>
      <div className="timeline-body">
        <div className="row">
          <Pill>{entry.type === "event" ? "Event" : "Decision"}</Pill>
          <span className="strong">{title}</span>
          {entry.type === "event" ? (
            entry.validation_state === "valid" ? null : (
              <Pill tone="attention">{validationLabel(entry.validation_state)}</Pill>
            )
          ) : (
            <span className="muted">
              {outcomeLabel(entry.outcome)}
              {entry.reason ? ` · ${reasonText(entry.reason)}` : ""}
            </span>
          )}
        </div>
        {entry.type === "event" ? <EventDetails entry={entry} /> : null}
        <Link to={`/w/${workspaceId}/traces/${entry.trace_id}`}>
          View trace<span className="visually-hidden"> of {title}</span>
        </Link>
      </div>
    </li>
  );
}

function show(value: unknown): string {
  return typeof value === "string" ? value : JSON.stringify(value);
}

function EventDetails({ entry }: { entry: Extract<TimelineEntry, { type: "event" }> }) {
  const properties = Object.entries(entry.properties);
  return (
    <>
      {entry.flags.length > 0 ? <p className="faint">Flags: {entry.flags.join(", ")}</p> : null}
      {properties.length > 0 ? (
        <details className="sample">
          <summary>
            Properties <span className="faint">({properties.length})</span>
          </summary>
          <dl className="facts">
            {properties.map(([name, value]) => (
              <div className="fact" key={name}>
                <dt className="mono">{name}</dt>
                <dd>{entry.masked.includes(name) ? <HiddenValue what="This property" /> : show(value)}</dd>
              </div>
            ))}
          </dl>
        </details>
      ) : null}
    </>
  );
}
