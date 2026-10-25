import { useState } from "react";

import { describeError } from "../../api";
import { Button, ErrorState, LoadingState, SelectField, TextField } from "../../ui";
import { useEventCatalog, useEventProperties } from "../events";
import { useFieldRegistry } from "../people";
import { MERGE_SYSTEM_VALUES } from "./model";

interface Props {
  workspaceId: string;
  /** Whether there is a field to put a tag into right now. */
  disabled: boolean;
  /** What the tag is put into, for the hint. */
  target: string;
  onInsert: (tag: string) => void;
}

/** The expression a field inserts: a default keeps the message readable when the value is missing. */
export function personTag(key: string, type: string): string {
  return type === "string" ? `{{ person.${key} | default("there") }}` : `{{ person.${key} }}`;
}

export function MergeTagPicker({ workspaceId, disabled, target, onInsert }: Props) {
  const fields = useFieldRegistry(workspaceId);
  const events = useEventCatalog(workspaceId);
  const [search, setSearch] = useState("");
  const [event, setEvent] = useState("");
  const properties = useEventProperties(workspaceId, event);
  const needle = search.trim().toLowerCase();

  if (fields.status === "loading" || events.status === "loading") {
    return <LoadingState label="Loading merge tags" />;
  }
  if (fields.status === "error" || events.status === "error") {
    const failed = fields.status === "error" ? fields : events;
    return (
      <ErrorState
        {...describeError(failed.status === "error" ? failed.error : null)}
        onRetry={() => {
          fields.reload();
          events.reload();
        }}
      />
    );
  }
  const traits = fields.data.filter((trait) => !trait.archived && trait.key.toLowerCase().includes(needle));
  const system = MERGE_SYSTEM_VALUES.filter((value) => value.label.toLowerCase().includes(needle) || value.expression.includes(needle));
  const names = properties.status === "ready" ? properties.data.filter((name) => name.toLowerCase().includes(needle)) : [];

  return (
    <section className="tp-picker" aria-label="Insert a merge tag">
      <h3 className="panel-title">Insert a merge tag</h3>
      <TextField
        label="Search fields and values"
        value={search}
        onChange={(entry) => setSearch(entry.target.value)}
        autoComplete="off"
        hint={disabled ? "Pick a field to edit first." : `Goes into ${target}.`}
      />
      <h4 className="field-label">Person fields</h4>
      {traits.length === 0 ? <p className="faint">No field matches.</p> : null}
      <ul className="list-plain">
        {traits.map((trait) => (
          <li key={trait.key}>
            <Button
              disabled={disabled}
              aria-label={`Insert person.${trait.key}`}
              onClick={() => onInsert(personTag(trait.key, trait.type))}
            >
              <span className="mono">person.{trait.key}</span> <span className="faint">{trait.type}</span>
            </Button>
          </li>
        ))}
      </ul>
      <h4 className="field-label">System values</h4>
      <ul className="list-plain">
        {system.map((value) => (
          <li key={value.expression}>
            <Button disabled={disabled} aria-label={`Insert ${value.expression}`} onClick={() => onInsert(`{{ ${value.expression} }}`)}>
              {value.label} <span className="mono faint">{value.expression}</span>
            </Button>
          </li>
        ))}
      </ul>
      <h4 className="field-label">Trigger event</h4>
      <SelectField label="Event" value={event} onChange={(entry) => setEvent(entry.target.value)}>
        <option value="">Choose an event</option>
        {events.data.map((entry) => (
          <option key={entry.name} value={entry.name}>
            {entry.name}
          </option>
        ))}
      </SelectField>
      {event !== "" && properties.status === "loading" ? <LoadingState label="Loading event properties" /> : null}
      {properties.status === "error" ? <ErrorState {...describeError(properties.error)} onRetry={properties.reload} /> : null}
      {event !== "" && properties.status === "ready" && names.length === 0 ? <p className="faint">No property matches.</p> : null}
      <ul className="list-plain">
        {names.map((name) => (
          <li key={name}>
            <Button disabled={disabled} aria-label={`Insert event.${name}`} onClick={() => onInsert(`{{ event.${name} }}`)}>
              <span className="mono">event.{name}</span>
            </Button>
          </li>
        ))}
      </ul>
    </section>
  );
}
