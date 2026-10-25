import { useState } from "react";

import { client, describeError, offeredDestinations, unwrap, useDestinationOptions, useResource, type components } from "../../api";
import {
  Button,
  ENGAGEMENT_PROPERTIES,
  ErrorState,
  InlineError,
  isMachineFilter,
  MACHINE_HINT,
  OPENED_EVENT,
  SelectField,
  TextField,
} from "../../ui";
import type { Catalog } from "../segments";
import { usePublishedTemplates } from "../templates";
import {
  DURATION_UNITS,
  MAX_DURATION_SECONDS,
  NIL_UUID,
  unitFor,
  unitSeconds,
  type DurationUnit,
  type FlowIssue,
  type Graph,
} from "./model";
import { issuesAt, issueText } from "./issues";

type Filter = components["schemas"]["SegmentPropertyFilter"];

/** The message of the first issue whose pointer starts with `prefix`, for a field to show. */
export function fieldError(issues: readonly FlowIssue[] | undefined, prefix: string, graph?: Graph): string | undefined {
  const first = issuesAt(issues, prefix)[0];
  return first ? issueText(first, graph) : undefined;
}

interface DurationFieldProps {
  legend: string;
  seconds: number;
  onChange: (seconds: number) => void;
  disabled: boolean;
  error?: string;
}

/** A length of time as an amount and a unit; the flow keeps it in seconds. */
export function DurationField({ legend, seconds, onChange, disabled, error }: DurationFieldProps) {
  const [unit, setUnit] = useState<DurationUnit>(unitFor(seconds));
  const [text, setText] = useState(String(seconds / unitSeconds(unit)));
  const [seen, setSeen] = useState(seconds);
  if (seen !== seconds) {
    // The seconds changed from outside, such as by an undo: show them in the unit in use.
    setSeen(seconds);
    if (Math.round(Number(text) * unitSeconds(unit)) !== seconds) setText(String(seconds / unitSeconds(unit)));
  }

  function change(nextText: string, nextUnit: DurationUnit) {
    setText(nextText);
    setUnit(nextUnit);
    const amount = Number(nextText);
    if (nextText.trim() === "" || !Number.isFinite(amount) || amount < 0) return;
    onChange(Math.min(MAX_DURATION_SECONDS + 1, Math.round(amount * unitSeconds(nextUnit))));
  }

  return (
    <fieldset className="fl-fieldset" aria-invalid={error ? true : undefined}>
      <legend className="field-label">{legend}</legend>
      <div className="fl-pair">
        <TextField
          label="Amount"
          type="number"
          inputMode="decimal"
          min={0}
          value={text}
          onChange={(event) => change(event.target.value, unit)}
          disabled={disabled}
          error={error}
        />
        <SelectField label="Unit" value={unit} onChange={(event) => change(text, event.target.value as DurationUnit)} disabled={disabled}>
          {DURATION_UNITS.map((name) => (
            <option key={name} value={name}>
              {name}s
            </option>
          ))}
        </SelectField>
      </div>
    </fieldset>
  );
}

interface PickProps {
  label: string;
  value: string;
  onChange: (value: string) => void;
  catalog: Catalog | null;
  disabled: boolean;
  error?: string;
  hint?: string;
}

/** An event of the workspace. A name the catalog does not know is kept and shown, so nothing is lost. */
export function EventSelect({ label, value, onChange, catalog, disabled, error, hint }: PickProps) {
  const known = catalog?.events.some((entry) => entry.name === value) ?? false;
  return (
    <SelectField label={label} value={value} onChange={(event) => onChange(event.target.value)} disabled={disabled || catalog === null} error={error} hint={hint}>
      <option value="">{catalog === null ? "Loading events" : "Choose an event"}</option>
      {value !== "" && !known && value !== OPENED_EVENT ? <option value={value}>{value}</option> : null}
      {catalog !== null && !catalog.events.some((entry) => entry.name === OPENED_EVENT) ? (
        <option value={OPENED_EVENT}>{OPENED_EVENT}</option>
      ) : null}
      {catalog?.events.map((entry) => (
        <option key={entry.name} value={entry.name}>
          {entry.name}
        </option>
      ))}
    </SelectField>
  );
}

/** A segment of the workspace; the placeholder id of a segment not chosen yet shows as unchosen. */
export function SegmentSelect({ label, value, onChange, catalog, disabled, error, hint }: PickProps) {
  const chosen = value === NIL_UUID ? "" : value;
  const known = catalog?.segments.some((entry) => entry.id === chosen) ?? false;
  return (
    <SelectField label={label} value={chosen} onChange={(event) => onChange(event.target.value || NIL_UUID)} disabled={disabled || catalog === null} error={error} hint={hint}>
      <option value="">{catalog === null ? "Loading segments" : "Choose a segment"}</option>
      {chosen !== "" && !known ? <option value={chosen}>A segment that is not in the list</option> : null}
      {catalog?.segments.map((entry) => (
        <option key={entry.id} value={entry.id}>
          {entry.name}
        </option>
      ))}
    </SelectField>
  );
}

/** A list of the workspace. */
export function ListSelect({ label, value, onChange, catalog, disabled, error, hint }: PickProps) {
  const chosen = value === NIL_UUID ? "" : value;
  const known = catalog?.lists.some((entry) => entry.id === chosen) ?? false;
  return (
    <SelectField label={label} value={chosen} onChange={(event) => onChange(event.target.value || NIL_UUID)} disabled={disabled || catalog === null} error={error} hint={hint}>
      <option value="">{catalog === null ? "Loading lists" : "Choose a list"}</option>
      {chosen !== "" && !known ? <option value={chosen}>A list that is not in the lists</option> : null}
      {catalog?.lists.map((entry) => (
        <option key={entry.id} value={entry.id}>
          {entry.name}
        </option>
      ))}
    </SelectField>
  );
}

/** A trait (field) of the workspace; any key already in use is kept and shown. */
export function TraitSelect({ label, value, onChange, catalog, disabled, error, hint }: PickProps) {
  const known = catalog?.traits.some((entry) => entry.key === value) ?? false;
  return (
    <SelectField label={label} value={value} onChange={(event) => onChange(event.target.value)} disabled={disabled || catalog === null} error={error} hint={hint}>
      <option value="">{catalog === null ? "Loading fields" : "Choose a field"}</option>
      {value !== "" && !known ? <option value={value}>{value}</option> : null}
      {catalog?.traits.map((entry) => (
        <option key={entry.key} value={entry.key}>
          {entry.key}
        </option>
      ))}
    </SelectField>
  );
}

type PropertyType = components["schemas"]["PropertyBody"]["type"];

/** The properties of an event as its newest published schema, or its draft, lists them. */
function useEventProperties(workspaceId: string, event: string): ReadonlyMap<string, PropertyType> {
  const detail = useResource(
    () =>
      event === ""
        ? Promise.resolve(null)
        : unwrap(client.GET("/api/v1/workspaces/{ws}/event-schemas/{name}", { params: { path: { ws: workspaceId, name: event } } })),
    [workspaceId, event],
  );
  const map = new Map<string, PropertyType>();
  for (const [name, type] of Object.entries(ENGAGEMENT_PROPERTIES[event] ?? {})) map.set(name, type);
  if (detail.status === "ready" && detail.data !== null) {
    for (const property of detail.data.versions[0]?.properties ?? detail.data.draft ?? []) {
      map.set(property.name, property.type);
    }
  }
  return map;
}

const FILTER_OPS: readonly { op: Filter["op"]; label: string; operand: boolean }[] = [
  { op: "eq", label: "is", operand: true },
  { op: "neq", label: "is not", operand: true },
  { op: "gt", label: "is more than", operand: true },
  { op: "gte", label: "is at least", operand: true },
  { op: "lt", label: "is less than", operand: true },
  { op: "lte", label: "is at most", operand: true },
  { op: "contains", label: "contains", operand: true },
  { op: "starts_with", label: "starts with", operand: true },
  { op: "exists", label: "is present", operand: false },
  { op: "not_exists", label: "is missing", operand: false },
];

export function operandText(value: unknown): string {
  if (value === undefined || value === null) return "";
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  return JSON.stringify(value);
}

function operandOf(text: string, type: PropertyType | undefined): unknown {
  if (type === "number" || type === "integer") {
    const number = Number(text);
    return text.trim() !== "" && Number.isFinite(number) ? number : text;
  }
  if (type === "boolean") return text === "true";
  return text;
}

interface FiltersProps {
  workspaceId: string;
  event: string;
  filters: readonly Filter[];
  onChange: (filters: Filter[]) => void;
  disabled: boolean;
  /** The pointer prefix of the filters in the issue list, such as `/filters` or `/trigger/filters`. */
  at: string;
  issues?: readonly FlowIssue[];
}

/** The property filters of an event match: which events of that name count. */
export function EventFilters({ workspaceId, event, filters, onChange, disabled, at, issues }: FiltersProps) {
  const properties = useEventProperties(workspaceId, event);

  function set(position: number, next: Filter) {
    onChange(filters.map((filter, index) => (index === position ? next : filter)));
  }

  return (
    <fieldset className="fl-fieldset">
      <legend className="field-label">Only events where</legend>
      {filters.length === 0 ? <p className="faint">Every event of this name counts.</p> : null}
      {event === OPENED_EVENT && filters.some(isMachineFilter) ? (
        <p className="field-hint">machine = false means {MACHINE_HINT}. Remove the filter to count every open.</p>
      ) : null}
      {filters.map((filter, position) => {
        const type = properties.get(filter.prop);
        const operand = FILTER_OPS.find((entry) => entry.op === filter.op)?.operand ?? true;
        const error = fieldError(issues, `${at}/${position}`);
        return (
          <div key={position} className="fl-filter" role="group" aria-label={`Filter ${position + 1}`}>
            {properties.size > 0 ? (
              <SelectField label="Property" value={filter.prop} onChange={(e) => set(position, { ...filter, prop: e.target.value })} disabled={disabled} error={error}>
                <option value="">Choose a property</option>
                {filter.prop !== "" && !properties.has(filter.prop) ? <option value={filter.prop}>{filter.prop}</option> : null}
                {[...properties.keys()].map((name) => (
                  <option key={name} value={name}>
                    {name}
                  </option>
                ))}
              </SelectField>
            ) : (
              <TextField label="Property" value={filter.prop} onChange={(e) => set(position, { ...filter, prop: e.target.value })} disabled={disabled} error={error} autoComplete="off" />
            )}
            <SelectField
              label="Condition"
              value={filter.op}
              onChange={(e) => {
                const op = e.target.value as Filter["op"];
                const takes = FILTER_OPS.find((entry) => entry.op === op)?.operand ?? true;
                set(position, takes ? { ...filter, op } : { prop: filter.prop, op });
              }}
              disabled={disabled}
            >
              {FILTER_OPS.map((entry) => (
                <option key={entry.op} value={entry.op}>
                  {entry.label}
                </option>
              ))}
            </SelectField>
            {operand ? (
              type === "boolean" ? (
                <SelectField
                  label="Value"
                  value={typeof filter.value === "boolean" ? String(filter.value) : ""}
                  onChange={(e) => set(position, { ...filter, value: e.target.value === "" ? "" : e.target.value === "true" })}
                  disabled={disabled}
                >
                  {typeof filter.value === "boolean" ? null : <option value="">Choose…</option>}
                  <option value="true">true</option>
                  <option value="false">false</option>
                </SelectField>
              ) : (
                <TextField label="Value" value={operandText(filter.value)} onChange={(e) => set(position, { ...filter, value: operandOf(e.target.value, type) })} disabled={disabled} autoComplete="off" />
              )
            ) : null}
            {disabled ? null : (
              <Button onClick={() => onChange(filters.filter((_, index) => index !== position))} aria-label={`Remove filter ${position + 1}`}>
                Remove
              </Button>
            )}
          </div>
        );
      })}
      {disabled ? null : (
        <Button onClick={() => onChange([...filters, { prop: "", op: "eq", value: "" }])}>Add a filter</Button>
      )}
    </fieldset>
  );
}

interface TemplateSelectProps {
  workspaceId: string;
  channel: "email" | "webhook";
  label: string;
  /** The template id, or the placeholder id, or empty for none. */
  value: string | undefined;
  onChange: (value: string | undefined) => void;
  disabled: boolean;
  /** Whether "none" is a valid choice. */
  optional?: boolean;
  error?: string;
  hint?: string;
}

/** A published template of the workspace. An id the list does not hold is kept and shown, so nothing is lost. */
export function TemplateSelect({ workspaceId, channel, label, value, onChange, disabled, optional = false, error, hint }: TemplateSelectProps) {
  const templates = usePublishedTemplates(workspaceId, channel);
  const chosen = value === undefined || value === NIL_UUID ? "" : value;
  if (templates.status === "error") {
    return (
      <div className="field">
        <span className="field-label">{label}</span>
        <InlineError {...describeError(templates.error)} />
        <Button onClick={templates.reload}>Try again</Button>
      </div>
    );
  }
  const list = templates.status === "ready" ? templates.data : [];
  const known = list.some((entry) => entry.id === chosen);
  return (
    <SelectField
      label={label}
      value={chosen}
      onChange={(event) => onChange(event.target.value === "" ? (optional ? undefined : NIL_UUID) : event.target.value)}
      disabled={disabled || templates.status === "loading"}
      error={error}
      hint={templates.status === "ready" && list.length === 0 ? "No template is published yet. Publish one under Templates." : hint}
    >
      <option value="">{templates.status === "loading" ? "Loading templates" : optional ? "No template, send the standard envelope" : "Choose a template"}</option>
      {chosen !== "" && !known && templates.status === "ready" ? <option value={chosen}>A template that is not published</option> : null}
      {list.map((entry) => (
        <option key={entry.id} value={entry.id}>
          {entry.name}
        </option>
      ))}
    </SelectField>
  );
}

interface DestinationSelectProps {
  workspaceId: string;
  label: string;
  value: string;
  /** What the step needs the destination to do: the list offers only destinations that can. */
  capability: "send" | "sync";
  onChange: (value: string) => void;
  disabled: boolean;
  error?: string;
  hint?: string;
}

const CAPABILITY_PHRASE = { send: "send email", sync: "sync profiles" } as const;

/** A live destination that can do what the step needs. A chosen one that is not offered keeps its name, or its id with a warning. */
export function DestinationSelect({ workspaceId, label, value, capability, onChange, disabled, error, hint }: DestinationSelectProps) {
  const destinations = useDestinationOptions(workspaceId);
  const chosen = value === NIL_UUID ? "" : value;
  if (destinations.status === "error") {
    return (
      <div className="field">
        <span className="field-label">{label}</span>
        <p className="faint">{chosen === "" ? "No destination chosen." : <span className="mono">{chosen}</span>}</p>
        <ErrorState {...describeError(destinations.error)} onRetry={destinations.reload} />
        {error ? <span className="field-error">{error}</span> : null}
      </div>
    );
  }
  const ready = destinations.status === "ready";
  const list = ready ? offeredDestinations(destinations.data, capability) : [];
  const offered = list.some((entry) => entry.id === chosen);
  const named = ready ? destinations.data.find((entry) => entry.id === chosen) : undefined;
  const notes = hint ? [hint] : [];
  if (ready && list.length === 0) notes.push(`No destination can ${CAPABILITY_PHRASE[capability]}. An admin adds one under destinations.`);
  if (ready && chosen !== "" && !offered) {
    notes.push(
      named
        ? `Warning: ${named.name} ${named.archived ? "is archived" : `cannot ${CAPABILITY_PHRASE[capability]}`}. Choose another.`
        : "Warning: the chosen destination is not in this workspace's list. Choose another.",
    );
  }
  return (
    <SelectField
      label={label}
      value={chosen}
      onChange={(event) => onChange(event.target.value || NIL_UUID)}
      disabled={disabled || destinations.status === "loading"}
      error={error}
      hint={notes.length > 0 ? notes.join(" ") : undefined}
    >
      <option value="">{destinations.status === "loading" ? "Loading destinations" : "Choose a destination"}</option>
      {ready && chosen !== "" && !offered ? <option value={chosen}>{named ? `${named.name}${named.archived ? " (archived)" : ""}` : chosen}</option> : null}
      {list.map((entry) => (
        <option key={entry.id} value={entry.id}>
          {entry.name}
        </option>
      ))}
    </SelectField>
  );
}
