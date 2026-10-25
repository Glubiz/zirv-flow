import { Button, SelectField, TextField } from "../../ui";
import { samePath, type Aggregate, type ConsentNode, type EventNode, type Path, type PropertyFilter, type TraitNode, type Window } from "./ast";
import { useBuilder } from "./builderContext";
import { traitTypes } from "./catalog";
import { issueMessage } from "./labels";
import { Operand, initialOperand } from "./Operand";
import {
  AGG_FNS,
  AGG_LABELS,
  aggNeedsProperty,
  aggType,
  OP_LABELS,
  operatorsFor,
  shapeOf,
  type AggFn,
  type Op,
  type ValueType,
} from "./operators";

/** The message of the first issue on `field` of the node at `path`, for the field to show. */
function useErrors(path: Path): (field: string) => string | undefined {
  const { issues } = useBuilder();
  return (field) => {
    const issue = issues.find((candidate) => samePath(candidate.path, path) && candidate.field === field);
    return issue ? issueMessage(issue.code) : undefined;
  };
}

function withValue<T extends object>(base: T, value: unknown): T {
  return value === undefined ? base : { ...base, value };
}

interface OpSelectProps {
  label: string;
  type: ValueType;
  op: Op;
  aggregate?: boolean;
  onChange: (op: Op) => void;
  error?: string;
}

function OpSelect({ label, type, op, aggregate = false, onChange, error }: OpSelectProps) {
  const { readOnly } = useBuilder();
  return (
    <SelectField label={label} value={op} onChange={(event) => onChange(event.target.value as Op)} disabled={readOnly} error={error}>
      {operatorsFor(type, aggregate).map((option) => (
        <option key={option} value={option}>
          {OP_LABELS[option]}
        </option>
      ))}
    </SelectField>
  );
}

/** The operator an operand type starts with when the old one does not apply. */
function keepOrFirst(type: ValueType, op: Op, aggregate = false): Op {
  const allowed = operatorsFor(type, aggregate);
  return allowed.includes(op) ? op : (allowed[0] ?? "eq");
}

export function TraitEditor({ node, path }: { node: TraitNode; path: Path }) {
  const { catalog, readOnly, edit } = useBuilder();
  const error = useErrors(path);
  const types = traitTypes(catalog.traits);
  const type = types.get(node.trait) ?? null;
  const known = catalog.traits.some((trait) => trait.key === node.trait);

  function pick(key: string) {
    const next = types.get(key) ?? null;
    if (next === type && next !== null) {
      edit(path, withValue({ trait: key, op: node.op }, node.value));
      return;
    }
    const op = next === null ? "eq" : keepOrFirst(next, "eq");
    edit(path, { trait: key, op });
  }

  return (
    <div className="condition-fields">
      <SelectField label="Field" value={node.trait} onChange={(event) => pick(event.target.value)} disabled={readOnly} error={error("trait")}>
        <option value="">Choose a field</option>
        {!known && node.trait !== "" ? <option value={node.trait}>{node.trait}</option> : null}
        {catalog.traits.map((trait) => (
          <option key={trait.key} value={trait.key}>
            {trait.archived ? `${trait.key} (archived)` : trait.key}
          </option>
        ))}
      </SelectField>
      {type === null ? null : (
        <>
          <OpSelect
            label="Comparison"
            type={type}
            op={node.op}
            error={error("op")}
            onChange={(op) =>
              edit(
                path,
                withValue(
                  { trait: node.trait, op },
                  shapeOf(type, op) === shapeOf(type, node.op) ? node.value : initialOperand(type, op),
                ),
              )
            }
          />
          <Operand
            label="Value"
            type={type}
            op={node.op}
            value={node.value}
            onChange={(value) => edit(path, withValue({ trait: node.trait, op: node.op }, value))}
            disabled={readOnly}
            error={error("value")}
          />
        </>
      )}
    </div>
  );
}

export function ListEditor({ node, path }: { node: { list: string }; path: Path }) {
  const { catalog, readOnly, edit } = useBuilder();
  const error = useErrors(path);
  return (
    <SelectField label="List" value={node.list} onChange={(event) => edit(path, { list: event.target.value })} disabled={readOnly} error={error("list")}>
      <option value="">Choose a list</option>
      {node.list !== "" && !catalog.lists.some((list) => list.id === node.list) ? <option value={node.list}>Unknown list</option> : null}
      {catalog.lists.map((list) => (
        <option key={list.id} value={list.id}>
          {list.name}
        </option>
      ))}
    </SelectField>
  );
}

export function SegmentEditor({ node, path }: { node: { segment: string }; path: Path }) {
  const { catalog, readOnly, edit, selfId } = useBuilder();
  const error = useErrors(path);
  const others = catalog.segments.filter((segment) => segment.id !== selfId);
  return (
    <SelectField
      label="Segment"
      value={node.segment}
      onChange={(event) => edit(path, { segment: event.target.value })}
      disabled={readOnly}
      error={error("segment")}
    >
      <option value="">Choose a segment</option>
      {node.segment !== "" && !others.some((segment) => segment.id === node.segment) ? (
        <option value={node.segment}>Unknown segment</option>
      ) : null}
      {others.map((segment) => (
        <option key={segment.id} value={segment.id}>
          {segment.name}
        </option>
      ))}
    </SelectField>
  );
}

const STATUSES: readonly { value: ConsentNode["consent"]["status"]; label: string }[] = [
  { value: "subscribed", label: "Subscribed" },
  { value: "unsubscribed", label: "Unsubscribed" },
  { value: "pending_confirmation", label: "Waiting for confirmation" },
  { value: "unknown", label: "No answer yet" },
];

const CHANNELS = ["email"];

export function ConsentEditor({ node, path }: { node: ConsentNode; path: Path }) {
  const { catalog, readOnly, edit } = useBuilder();
  const error = useErrors(path);
  const { status, topic } = node.consent;
  // A segment always names its channel; only a consent policy rule leaves it out.
  const channel = node.consent.channel ?? "";
  const topics = catalog.topics.filter((entry) => entry.channel === channel && !entry.archived);

  function change(next: Partial<{ channel: string; status: ConsentNode["consent"]["status"]; topic: string }>) {
    const merged = { channel, status, topic, ...next };
    edit(path, {
      consent: {
        channel: merged.channel,
        status: merged.status,
        ...(merged.topic === undefined || merged.topic === "" ? {} : { topic: merged.topic }),
      },
    });
  }

  return (
    <div className="condition-fields">
      <SelectField
        label="Channel"
        value={channel}
        onChange={(event) => change({ channel: event.target.value, topic: "" })}
        disabled={readOnly}
        error={error("consent.channel")}
      >
        {CHANNELS.includes(channel) ? null : <option value={channel}>{channel}</option>}
        {CHANNELS.map((option) => (
          <option key={option} value={option}>
            {option === "email" ? "Email" : option}
          </option>
        ))}
      </SelectField>
      <SelectField
        label="Topic"
        value={topic ?? ""}
        onChange={(event) => change({ topic: event.target.value })}
        disabled={readOnly}
        error={error("consent.topic")}
      >
        <option value="">The whole channel</option>
        {topic !== undefined && topic !== "" && !topics.some((entry) => entry.key === topic) ? <option value={topic}>{topic}</option> : null}
        {topics.map((entry) => (
          <option key={entry.id} value={entry.key}>
            {entry.name}
          </option>
        ))}
      </SelectField>
      <SelectField
        label="Status"
        value={status}
        onChange={(event) => change({ status: event.target.value as ConsentNode["consent"]["status"] })}
        disabled={readOnly}
        error={error("consent.status")}
      >
        {STATUSES.map((option) => (
          <option key={option.value} value={option.value}>
            {option.label}
          </option>
        ))}
      </SelectField>
    </div>
  );
}

const IDENTIFIER_KINDS = [
  { value: "email", label: "Email address" },
  { value: "phone", label: "Phone number" },
  { value: "anonymous_id", label: "Anonymous ID" },
  { value: "external", label: "External ID" },
];

export function IdentifierEditor({ node, path }: { node: { identifier: string }; path: Path }) {
  const { readOnly, edit } = useBuilder();
  const error = useErrors(path);
  const external = node.identifier.startsWith("external:");
  const kind = external ? "external" : node.identifier;
  const namespace = external ? node.identifier.slice("external:".length) : "";
  return (
    <div className="condition-fields">
      <SelectField
        label="Identifier kind"
        value={kind}
        onChange={(event) => edit(path, { identifier: event.target.value === "external" ? "external:" : event.target.value })}
        disabled={readOnly}
        error={external ? undefined : error("identifier")}
        hint="The person has an identifier of this kind"
      >
        {IDENTIFIER_KINDS.some((option) => option.value === kind) ? null : <option value={kind}>{kind}</option>}
        {IDENTIFIER_KINDS.map((option) => (
          <option key={option.value} value={option.value}>
            {option.label}
          </option>
        ))}
      </SelectField>
      {external ? (
        <TextField
          label="System name"
          hint="Lowercase, such as crm."
          value={namespace}
          onChange={(event) => edit(path, { identifier: `external:${event.target.value}` })}
          disabled={readOnly}
          error={error("identifier")}
          autoComplete="off"
        />
      ) : null}
    </div>
  );
}

function withoutEmpty(node: EventNode): EventNode {
  const next: EventNode = { event: node.event };
  if (node.where !== undefined && node.where.length > 0) next.where = node.where;
  if (node.agg !== undefined) next.agg = node.agg;
  if (node.window !== undefined) next.window = node.window;
  return next;
}

type WindowKind = "" | "last" | "between" | "since_trait";

function windowKind(window: Window | undefined): WindowKind {
  if (window === undefined) return "";
  if ("last" in window) return "last";
  if ("between" in window) return "between";
  return "since_trait";
}

/** The property filters of an event condition: the `where` list, shared with the forwarding rules. */
export function PropertyFilters({
  node,
  path,
  update,
}: {
  node: EventNode;
  path: Path;
  update: (change: Partial<EventNode>) => void;
}) {
  const { readOnly, eventTypes } = useBuilder();
  const error = useErrors(path);
  const properties = eventTypes.get(node.event);
  const filters = node.where ?? [];
  const names = [...(properties?.keys() ?? [])];

  function setFilter(position: number, next: PropertyFilter) {
    update({ where: filters.map((filter, at) => (at === position ? next : filter)) });
  }

  return (
    <fieldset className="condition-group">
      <legend className="field-label">Property filters</legend>
      {filters.length === 0 ? <p className="faint">Every event of this name counts.</p> : null}
      {filters.map((filter, at) => {
        const type = properties?.get(filter.prop) ?? null;
        const label = `filter ${at + 1}`;
        return (
          <div key={at} className="filter-row" role="group" aria-label={`Property filter ${at + 1}`}>
            <SelectField
              label={`Property of ${label}`}
              value={filter.prop}
              onChange={(event) => {
                const next = properties?.get(event.target.value) ?? null;
                setFilter(at, { prop: event.target.value, op: next === null ? "eq" : keepOrFirst(next, "eq") });
              }}
              disabled={readOnly}
              error={error(`where.${at}.prop`)}
            >
              <option value="">Choose a property</option>
              {filter.prop !== "" && !names.includes(filter.prop) ? <option value={filter.prop}>{filter.prop}</option> : null}
              {names.map((name) => (
                <option key={name} value={name}>
                  {name}
                </option>
              ))}
            </SelectField>
            {type === null ? null : (
              <>
                <OpSelect
                  label={`Comparison of ${label}`}
                  type={type}
                  op={filter.op}
                  error={error(`where.${at}.op`)}
                  onChange={(op) =>
                    setFilter(at, withValue({ prop: filter.prop, op }, shapeOf(type, op) === shapeOf(type, filter.op) ? filter.value : initialOperand(type, op)))
                  }
                />
                <Operand
                  label={`Value of ${label}`}
                  type={type}
                  op={filter.op}
                  value={filter.value}
                  onChange={(value) => setFilter(at, withValue({ prop: filter.prop, op: filter.op }, value))}
                  disabled={readOnly}
                  error={error(`where.${at}.value`)}
                />
              </>
            )}
            {readOnly ? null : (
              <Button onClick={() => update({ where: filters.filter((_, index) => index !== at) })}>
                Remove <span className="visually-hidden">{label}</span>
              </Button>
            )}
          </div>
        );
      })}
      {readOnly ? null : (
        <Button onClick={() => update({ where: [...filters, { prop: "", op: "eq" }] })} disabled={node.event === ""}>
          Add a property filter
        </Button>
      )}
    </fieldset>
  );
}

export function EventEditor({ node, path }: { node: EventNode; path: Path }) {
  const { catalog, readOnly, edit, eventTypes } = useBuilder();
  const error = useErrors(path);
  const properties = eventTypes.get(node.event);
  const timeTraits = catalog.traits.filter((trait) => trait.type === "timestamp" || trait.type === "date");
  const names = [...(properties?.keys() ?? [])];
  const numeric = names.filter((name) => properties?.get(name) === "number" || properties?.get(name) === "integer");

  function update(change: Partial<EventNode>) {
    edit(path, withoutEmpty({ ...node, ...change }));
  }

  function setWindow(kind: WindowKind) {
    if (kind === "") {
      update({ window: undefined });
    } else if (kind === "last") {
      update({ window: { last: "" } });
    } else if (kind === "between") {
      update({ window: { between: ["", ""] } });
    } else {
      update({ window: { since_trait: "" } });
    }
  }


  function setAgg(next: Aggregate | undefined) {
    update({ agg: next });
  }

  const window = node.window;
  const agg = node.agg;

  return (
    <div className="condition-fields">
      <SelectField
        label="Event"
        value={node.event}
        onChange={(event) => edit(path, { event: event.target.value })}
        disabled={readOnly}
        error={error("event")}
      >
        <option value="">Choose an event</option>
        {node.event !== "" && !catalog.events.some((entry) => entry.name === node.event) ? <option value={node.event}>{node.event}</option> : null}
        {catalog.events.map((entry) => (
          <option key={entry.name} value={entry.name}>
            {entry.name}
          </option>
        ))}
      </SelectField>

      <SelectField
        label="Time window"
        value={windowKind(window)}
        onChange={(event) => setWindow(event.target.value as WindowKind)}
        disabled={readOnly}
        error={error("window")}
      >
        <option value="">Any time</option>
        <option value="last">In the last</option>
        <option value="between">Between two moments</option>
        <option value="since_trait">Since a field's date</option>
      </SelectField>
      {window !== undefined && "last" in window ? (
        <Operand
          label="Window length"
          type="timestamp"
          op="within_last"
          value={window.last}
          onChange={(value) => update({ window: { last: typeof value === "string" ? value : "" } })}
          disabled={readOnly}
          error={error("window")}
        />
      ) : null}
      {window !== undefined && "between" in window ? (
        <Operand
          label="Window"
          type="timestamp"
          op="between"
          value={window.between}
          onChange={(value) => update({ window: { between: Array.isArray(value) ? (value as string[]) : ["", ""] } })}
          disabled={readOnly}
          error={error("window")}
        />
      ) : null}
      {window !== undefined && "since_trait" in window ? (
        <SelectField
          label="Since the date in"
          value={window.since_trait}
          onChange={(event) => update({ window: { since_trait: event.target.value } })}
          disabled={readOnly}
          error={error("window")}
        >
          <option value="">Choose a field</option>
          {timeTraits.map((trait) => (
            <option key={trait.key} value={trait.key}>
              {trait.key}
            </option>
          ))}
        </SelectField>
      ) : null}

      <PropertyFilters node={node} path={path} update={update} />

      <fieldset className="condition-group">
        <legend className="field-label">Summary</legend>
        {agg === undefined ? (
          <>
            <p className="faint">With no summary, one matching event is enough.</p>
            {readOnly ? null : (
              <Button onClick={() => setAgg({ fn: "count", op: "gte", value: undefined })} disabled={node.event === ""}>
                Add a summary
              </Button>
            )}
          </>
        ) : (
          <AggregateEditor agg={agg} names={numeric} onChange={setAgg} error={error} />
        )}
      </fieldset>
    </div>
  );
}

function AggregateEditor({
  agg,
  names,
  onChange,
  error,
}: {
  agg: Aggregate;
  names: readonly string[];
  onChange: (next: Aggregate | undefined) => void;
  error: (field: string) => string | undefined;
}) {
  const { readOnly } = useBuilder();
  const type = aggType(agg.fn);
  const make = (fn: AggFn, op: Op, value: unknown, prop?: string): Aggregate => {
    const base: Aggregate = { fn, op, value };
    return aggNeedsProperty(fn) && prop !== undefined && prop !== "" ? { ...base, prop } : base;
  };

  return (
    <div className="stack-tight">
      <SelectField
        label="Summary of the events"
        value={agg.fn}
        onChange={(event) => {
          const fn = event.target.value as AggFn;
          onChange(make(fn, keepOrFirst(aggType(fn), agg.op, true), aggType(fn) === type ? agg.value : undefined, agg.prop));
        }}
        disabled={readOnly}
        error={error("agg.fn")}
      >
        {AGG_FNS.map((fn) => (
          <option key={fn} value={fn}>
            {AGG_LABELS[fn]}
          </option>
        ))}
      </SelectField>
      {aggNeedsProperty(agg.fn) ? (
        <SelectField
          label="Property to summarise"
          value={agg.prop ?? ""}
          onChange={(event) => onChange(make(agg.fn, agg.op, agg.value, event.target.value))}
          disabled={readOnly}
          error={error("agg.prop")}
        >
          <option value="">Choose a number property</option>
          {agg.prop !== undefined && !names.includes(agg.prop) ? <option value={agg.prop}>{agg.prop}</option> : null}
          {names.map((name) => (
            <option key={name} value={name}>
              {name}
            </option>
          ))}
        </SelectField>
      ) : null}
      <OpSelect
        label="Summary comparison"
        type={type}
        op={agg.op}
        aggregate
        error={error("agg.op")}
        onChange={(op) => onChange(make(agg.fn, op, shapeOf(type, op) === shapeOf(type, agg.op) ? agg.value : initialOperand(type, op), agg.prop))}
      />
      <Operand
        label="Summary value"
        type={type}
        op={agg.op}
        value={agg.value}
        onChange={(value) => onChange(make(agg.fn, agg.op, value, agg.prop))}
        disabled={readOnly}
        error={error("agg.value")}
      />
      {readOnly ? null : <Button onClick={() => onChange(undefined)}>Remove the summary</Button>}
    </div>
  );
}
