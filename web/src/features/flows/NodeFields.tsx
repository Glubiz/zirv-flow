import { useState } from "react";

import { Button, SelectField, TextField } from "../../ui";
import { ConditionEditor, type Catalog } from "../segments";
import {
  DurationField,
  EventFilters,
  EventSelect,
  fieldError,
  ListSelect,
  operandText,
  TraitSelect,
} from "./fields";
import { conditionIssues } from "./issues";
import {
  MAX_ARMS,
  MAX_CONDITIONS,
  MIN_ARMS,
  type Condition,
  type FlowIssue,
  type FlowNode,
  type Graph,
} from "./model";

type Of<T extends FlowNode["type"]> = Extract<FlowNode, { type: T }>;

const WEEKDAYS = [
  { id: "mon", label: "Mon" },
  { id: "tue", label: "Tue" },
  { id: "wed", label: "Wed" },
  { id: "thu", label: "Thu" },
  { id: "fri", label: "Fri" },
  { id: "sat", label: "Sat" },
  { id: "sun", label: "Sun" },
] as const;

interface FieldsProps {
  workspaceId: string;
  graph: Graph;
  node: FlowNode;
  issues: readonly FlowIssue[];
  catalog: Catalog | null;
  readOnly: boolean;
  /** Replaces the step; `key` joins quick edits of one field into a single step of undo. */
  onChange: (next: FlowNode, key?: string) => void;
  /** Replaces the step with a copy that lost its `index`th case or arm, and moves the wires after it. */
  onRemovePort: (next: FlowNode, prefix: "case_" | "arm_", index: number) => void;
}

/** The fields of one step, whatever its kind. */
export function NodeFields(props: FieldsProps) {
  const { node } = props;
  switch (node.type) {
    case "wait.duration":
      return <WaitDuration {...props} node={node} />;
    case "wait.until_time":
      return <WaitUntilTime {...props} node={node} />;
    case "wait.until_event":
      return <WaitUntilEvent {...props} node={node} />;
    case "branch":
      return <Branch {...props} node={node} />;
    case "split.random":
      return <Split {...props} node={node} />;
    case "action.webhook":
      return <Webhook {...props} node={node} />;
    case "action.update_trait":
      return <UpdateTrait {...props} node={node} />;
    case "action.list":
      return <UpdateList {...props} node={node} />;
    case "goal":
      return <Goal {...props} node={node} />;
    case "exit":
      return <p className="muted">A person who reaches this step leaves the flow. Nothing comes after it.</p>;
  }
}

function WaitDuration({ node, issues, graph, readOnly, onChange }: FieldsProps & { node: Of<"wait.duration"> }) {
  return (
    <DurationField
      legend="Wait for"
      seconds={node.seconds}
      onChange={(seconds) => onChange({ ...node, seconds }, `${node.id}:seconds`)}
      disabled={readOnly}
      error={fieldError(issues, "/seconds", graph)}
    />
  );
}

function WaitUntilTime({ node, issues, graph, readOnly, onChange }: FieldsProps & { node: Of<"wait.until_time"> }) {
  const when = node.when;
  return (
    <div className="stack">
      <SelectField
        label="Wait until"
        value={when.kind}
        onChange={(event) =>
          onChange({
            ...node,
            when: event.target.value === "date_time" ? { kind: "date_time", at: "" } : { kind: "time_of_day", time: "09:00" },
          })
        }
        disabled={readOnly}
        hint="Times are in the person's time zone, and the workspace's when the person has none."
      >
        <option value="time_of_day">The next time of day</option>
        <option value="date_time">A date and time</option>
      </SelectField>
      {when.kind === "time_of_day" ? (
        <>
          <TextField
            label="Time of day"
            type="time"
            value={when.time}
            onChange={(event) => onChange({ ...node, when: { ...when, time: event.target.value } }, `${node.id}:time`)}
            disabled={readOnly}
            error={fieldError(issues, "/when/time", graph)}
          />
          <fieldset className="fl-fieldset" aria-invalid={fieldError(issues, "/when/weekdays", graph) ? true : undefined}>
            <legend className="field-label">On these days</legend>
            <div className="fl-days">
              {WEEKDAYS.map((day) => {
                const days = when.weekdays ?? [];
                return (
                  <label key={day.id} className="check">
                    <input
                      type="checkbox"
                      checked={days.includes(day.id)}
                      disabled={readOnly}
                      onChange={(event) => {
                        const next = event.target.checked ? [...days, day.id] : days.filter((entry) => entry !== day.id);
                        const ordered = WEEKDAYS.map((entry) => entry.id).filter((id) => next.includes(id));
                        onChange({ ...node, when: ordered.length === 0 ? { kind: "time_of_day", time: when.time } : { ...when, weekdays: ordered } });
                      }}
                    />
                    {day.label}
                  </label>
                );
              })}
            </div>
            <p className="field-hint">With none ticked, every day counts.</p>
            {fieldError(issues, "/when/weekdays", graph) ? <p className="field-error">{fieldError(issues, "/when/weekdays", graph)}</p> : null}
          </fieldset>
        </>
      ) : (
        <TextField
          label="Date and time"
          type="datetime-local"
          step={1}
          value={when.at}
          onChange={(event) => {
            const value = event.target.value;
            onChange({ ...node, when: { kind: "date_time", at: value !== "" && value.length === 16 ? `${value}:00` : value } }, `${node.id}:at`);
          }}
          disabled={readOnly}
          error={fieldError(issues, "/when/at", graph)}
        />
      )}
    </div>
  );
}

function WaitUntilEvent({ node, issues, graph, catalog, workspaceId, readOnly, onChange }: FieldsProps & { node: Of<"wait.until_event"> }) {
  return (
    <div className="stack">
      <EventSelect
        label="Event"
        value={node.event}
        onChange={(event) => onChange({ ...node, event, filters: [] })}
        catalog={catalog}
        disabled={readOnly}
        error={fieldError(issues, "/event", graph)}
      />
      <EventFilters
        workspaceId={workspaceId}
        event={node.event}
        filters={node.filters ?? []}
        onChange={(filters) => onChange({ ...node, filters }, `${node.id}:filters`)}
        disabled={readOnly}
        at="/filters"
        issues={issues}
      />
      <DurationField
        legend="Give up waiting after"
        seconds={node.timeout_seconds}
        onChange={(timeout_seconds) => onChange({ ...node, timeout_seconds }, `${node.id}:timeout`)}
        disabled={readOnly}
        error={fieldError(issues, "/timeout_seconds", graph)}
      />
    </div>
  );
}

function Branch({ node, issues, readOnly, workspaceId, onChange, onRemovePort }: FieldsProps & { node: Of<"branch"> }) {
  function setCondition(index: number, next: Condition) {
    onChange({ ...node, conditions: node.conditions.map((condition, at) => (at === index ? next : condition)) }, `${node.id}:condition:${index}`);
  }
  return (
    <div className="stack">
      <p className="muted">
        The first condition a person meets decides which way they go. People who meet none go the else way.
      </p>
      {node.conditions.map((condition, index) => (
        <fieldset key={index} className="fl-fieldset" aria-label={`Condition ${index + 1}`}>
          <legend className="field-label">{node.conditions.length === 1 ? "Condition" : `Case ${index + 1}`}</legend>
          <ConditionEditor
            workspaceId={workspaceId}
            value={condition}
            onChange={(next) => setCondition(index, next)}
            readOnly={readOnly}
            issues={conditionIssues(issues, index)}
          />
          {!readOnly && node.conditions.length > 1 ? (
            <Button
              aria-label={`Remove case ${index + 1}`}
              onClick={() => onRemovePort({ ...node, conditions: node.conditions.filter((_, at) => at !== index) }, "case_", index)}
            >
              Remove this case
            </Button>
          ) : null}
        </fieldset>
      ))}
      {!readOnly && node.conditions.length < MAX_CONDITIONS ? (
        <Button onClick={() => onChange({ ...node, conditions: [...node.conditions, { trait: "", op: "eq" }] })}>Add a case</Button>
      ) : null}
    </div>
  );
}

function Split({ node, issues, graph, readOnly, onChange, onRemovePort }: FieldsProps & { node: Of<"split.random"> }) {
  const total = node.arms.reduce((sum, arm) => sum + arm.weight, 0);
  const armError = fieldError(issues, "/arms", graph);

  function even() {
    const count = node.arms.length;
    const base = Math.floor(100 / count);
    const arms = node.arms.map((_, index) => ({ weight: base + (index < 100 - base * count ? 1 : 0) }));
    onChange({ ...node, arms });
  }

  return (
    <div className="stack">
      <p className="muted">Each person goes down one arm, chosen at random in proportion to the weights.</p>
      {node.arms.map((arm, index) => (
        <div key={index} className="fl-pair">
          <WeightField
            label={`Weight of arm ${index + 1}`}
            weight={arm.weight}
            onChange={(weight) =>
              onChange({ ...node, arms: node.arms.map((entry, at) => (at === index ? { weight } : entry)) }, `${node.id}:arm:${index}`)
            }
            disabled={readOnly}
            error={fieldError(issues, `/arms/${index}`, graph)}
          />
          {!readOnly && node.arms.length > MIN_ARMS ? (
            <Button
              aria-label={`Remove arm ${index + 1}`}
              onClick={() => onRemovePort({ ...node, arms: node.arms.filter((_, at) => at !== index) }, "arm_", index)}
            >
              Remove
            </Button>
          ) : null}
        </div>
      ))}
      <p className={total === 100 ? "faint" : "field-error"} role="status">
        The weights add up to {total}. They must add up to 100.
      </p>
      {armError && total === 100 ? <p className="field-error">{armError}</p> : null}
      {readOnly ? null : (
        <div className="row">
          {node.arms.length < MAX_ARMS ? <Button onClick={() => onChange({ ...node, arms: [...node.arms, { weight: 1 }] })}>Add an arm</Button> : null}
          <Button onClick={even}>Split evenly</Button>
        </div>
      )}
    </div>
  );
}

/** The range of an arm's weight that the API accepts. */
const MIN_WEIGHT = 1;
const MAX_WEIGHT = 100;

interface WeightFieldProps {
  label: string;
  weight: number;
  onChange: (weight: number) => void;
  disabled: boolean;
  error?: string;
}

/**
 * The weight of one arm. What is typed is kept as typed while it is being typed; a whole number is
 * brought into 1 to 100 and saved, and an empty or unreadable field saves nothing and says what is wanted.
 */
function WeightField({ label, weight, onChange, disabled, error }: WeightFieldProps) {
  const [text, setText] = useState(String(weight));
  const [seen, setSeen] = useState(weight);
  const [bad, setBad] = useState(false);
  if (seen !== weight) {
    // The weight changed from outside, such as by an undo or by splitting evenly.
    setSeen(weight);
    const typed = Number(text);
    if (bad || text.trim() === "" || Math.min(MAX_WEIGHT, Math.max(MIN_WEIGHT, Math.round(typed))) !== weight) {
      setText(String(weight));
      setBad(false);
    }
  }

  function change(next: string) {
    setText(next);
    const typed = Number(next);
    if (next.trim() === "" || !Number.isFinite(typed)) {
      setBad(true);
      return;
    }
    setBad(false);
    const clamped = Math.min(MAX_WEIGHT, Math.max(MIN_WEIGHT, Math.round(typed)));
    if (clamped !== typed) setText(String(clamped));
    setSeen(clamped);
    if (clamped !== weight) onChange(clamped);
  }

  return (
    <TextField
      label={label}
      type="number"
      inputMode="numeric"
      min={MIN_WEIGHT}
      max={MAX_WEIGHT}
      value={text}
      onChange={(event) => change(event.target.value)}
      onBlur={() => {
        if (bad) {
          setText(String(weight));
          setBad(false);
        }
      }}
      disabled={disabled}
      error={bad ? `Enter a whole number from ${MIN_WEIGHT} to ${MAX_WEIGHT}.` : error}
    />
  );
}

function Webhook({ node, issues, graph, catalog, readOnly, onChange }: FieldsProps & { node: Of<"action.webhook"> }) {
  const keys = node.trait_keys ?? [];
  const offered = catalog?.traits.filter((trait) => !keys.includes(trait.key)) ?? [];
  const [picked, setPicked] = useState("");
  return (
    <div className="stack">
      <TextField
        label="URL"
        mono
        value={node.url}
        onChange={(event) => onChange({ ...node, url: event.target.value }, `${node.id}:url`)}
        disabled={readOnly}
        autoComplete="off"
        spellCheck={false}
        error={fieldError(issues, "/url", graph)}
      />
      <div className="field">
        <span className="field-label" id={`${node.id}-keys`}>
          Traits sent with each call
        </span>
        {keys.length === 0 ? <p className="faint">None. Only the standard envelope is sent.</p> : null}
        <ul className="fl-chips" aria-labelledby={`${node.id}-keys`}>
          {keys.map((key, index) => (
            <li key={key} className="fl-chip">
              <span className="mono">{key}</span>
              {readOnly ? null : (
                <button
                  type="button"
                  className="fl-chip-remove"
                  aria-label={`Remove ${key}`}
                  onClick={() => onChange({ ...node, trait_keys: keys.filter((_, at) => at !== index) })}
                >
                  ×
                </button>
              )}
            </li>
          ))}
        </ul>
        {fieldError(issues, "/trait_keys", graph) ? <p className="field-error">{fieldError(issues, "/trait_keys", graph)}</p> : null}
        {readOnly ? null : (
          <SelectField
            label="Add a trait"
            value={picked}
            onChange={(event) => {
              if (event.target.value === "") return;
              setPicked("");
              onChange({ ...node, trait_keys: [...keys, event.target.value] });
            }}
            disabled={catalog === null || offered.length === 0}
          >
            <option value="">{offered.length === 0 && catalog !== null ? "No more traits" : "Choose a trait"}</option>
            {offered.map((trait) => (
              <option key={trait.key} value={trait.key}>
                {trait.key}
              </option>
            ))}
          </SelectField>
        )}
      </div>
      <div className="fl-note">
        <div className="strong">What gets sent</div>
        <p>
          A signed JSON envelope with the flow, version, step, enrollment and person ids, the trace id, an idempotency key and the time.
          The result arrives as <span className="mono">webhook.succeeded</span> or <span className="mono">webhook.failed</span>.
        </p>
      </div>
    </div>
  );
}

function UpdateTrait({ node, issues, graph, catalog, readOnly, onChange }: FieldsProps & { node: Of<"action.update_trait"> }) {
  const type = catalog?.traits.find((trait) => trait.key === node.key)?.type;
  const change = node.change;
  return (
    <div className="stack">
      <TraitSelect
        label="Trait"
        value={node.key}
        onChange={(key) => onChange({ ...node, key })}
        catalog={catalog}
        disabled={readOnly}
        error={fieldError(issues, "/key", graph)}
      />
      <SelectField
        label="Change"
        value={change.op}
        onChange={(event) => {
          const op = event.target.value;
          onChange({ ...node, change: op === "increment" ? { op: "increment", by: 1 } : op === "unset" ? { op: "unset" } : { op: "set", value: "" } });
        }}
        disabled={readOnly}
      >
        <option value="set">Set to a value</option>
        <option value="increment">Add to a number</option>
        <option value="unset">Clear what this flow set</option>
      </SelectField>
      {change.op === "set" ? (
        type === "boolean" ? (
          <SelectField
            label="Value"
            value={typeof change.value === "boolean" ? String(change.value) : ""}
            onChange={(event) =>
              onChange({ ...node, change: { op: "set", value: event.target.value === "" ? "" : event.target.value === "true" } })
            }
            disabled={readOnly}
            error={fieldError(issues, "/change/value", graph)}
          >
            {typeof change.value === "boolean" ? null : <option value="">Choose…</option>}
            <option value="true">true</option>
            <option value="false">false</option>
          </SelectField>
        ) : (
          <TextField
            label="Value"
            type={type === "number" || type === "integer" ? "number" : "text"}
            value={operandText(change.value)}
            onChange={(event) => {
              const text = event.target.value;
              const number = Number(text);
              const value = (type === "number" || type === "integer") && text.trim() !== "" && Number.isFinite(number) ? number : text;
              onChange({ ...node, change: { op: "set", value } }, `${node.id}:value`);
            }}
            disabled={readOnly}
            autoComplete="off"
            error={fieldError(issues, "/change/value", graph)}
          />
        )
      ) : null}
      {change.op === "increment" ? (
        <TextField
          label="Add"
          type="number"
          value={String(change.by)}
          onChange={(event) => {
            const by = Number(event.target.value);
            if (Number.isFinite(by)) onChange({ ...node, change: { op: "increment", by } }, `${node.id}:by`);
          }}
          disabled={readOnly}
          error={fieldError(issues, "/change/by", graph)}
        />
      ) : null}
    </div>
  );
}

function UpdateList({ node, issues, graph, catalog, readOnly, onChange }: FieldsProps & { node: Of<"action.list"> }) {
  return (
    <div className="stack">
      <SelectField label="Change" value={node.change} onChange={(event) => onChange({ ...node, change: event.target.value === "remove" ? "remove" : "add" })} disabled={readOnly}>
        <option value="add">Add the person to</option>
        <option value="remove">Remove the person from</option>
      </SelectField>
      <ListSelect
        label="List"
        value={node.list_id}
        onChange={(list_id) => onChange({ ...node, list_id })}
        catalog={catalog}
        disabled={readOnly}
        error={fieldError(issues, "/list_id", graph)}
      />
    </div>
  );
}

function Goal({ node, readOnly, onChange }: FieldsProps & { node: Of<"goal"> }) {
  return (
    <div className="stack">
      <p className="muted">Records that the person reached the goal.</p>
      <label className="check">
        <input
          type="checkbox"
          checked={node.exit === true}
          disabled={readOnly}
          onChange={(event) => onChange(event.target.checked ? { ...node, exit: true } : { id: node.id, type: "goal" })}
        />
        Also end the flow for the person
      </label>
      <p className="field-hint">Ending the flow here means this step has nothing after it.</p>
    </div>
  );
}
