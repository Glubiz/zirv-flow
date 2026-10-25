import { useState } from "react";

import { client, describeError, unwrap, useResource, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError, SelectField, TextField } from "../../ui";
import { PropertyFilterEditor, type PropertyFilter } from "../segments";
import { kindOf, type Destination, type DestinationKind, type Rule } from "./labels";
import { mappingFromRows, newRow, rowsFromMapping, SOURCES, type MappingRow, type SourceId, type ValueType } from "./mapping";

const MAX_NAME_CHARS = 128;

interface RuleDialogProps {
  workspaceId: string;
  /** The destinations a rule may name: those whose type carries events. */
  destinations: Destination[];
  existing?: Rule;
  onClose: () => void;
  onSaved: () => void;
}

/** Creates or changes a forwarding rule: the event, a typed filter, and the mapping of the payload. */
export function RuleDialog({ workspaceId, destinations, existing, onClose, onSaved }: RuleDialogProps) {
  const events = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/event-schemas", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const [name, setName] = useState(existing?.name ?? "");
  const [destinationId, setDestinationId] = useState(existing?.destination_id ?? destinations[0]?.id ?? "");
  const [eventName, setEventName] = useState(existing?.event_name ?? "");
  const [active, setActive] = useState(existing?.active ?? true);
  const [filter, setFilter] = useState<PropertyFilter[]>((existing?.filter ?? []) as PropertyFilter[]);
  const [rows, setRows] = useState<MappingRow[]>(existing ? rowsFromMapping(existing.mapping) : [{ ...newRow(), key: "id" }]);
  const [rowErrors, setRowErrors] = useState<Record<number, string>>({});
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [busy, setBusy] = useState(false);

  function change(at: number, next: Partial<MappingRow>) {
    setRows(rows.map((row, index) => (index === at ? { ...row, ...next } : row)));
  }

  async function save() {
    const mapping = mappingFromRows(rows);
    if (!mapping.ok) {
      setRowErrors(mapping.errors);
      return;
    }
    setRowErrors({});
    setBusy(true);
    setFailure(null);
    try {
      const body = { name: name.trim(), event_name: eventName.trim(), filter, mapping: mapping.mapping, active };
      if (existing) {
        await unwrap(
          client.PATCH("/api/v1/workspaces/{ws}/forwarding/rules/{id}", {
            params: { path: { ws: workspaceId, id: existing.id } },
            body: { ...body, destination_id: destinationId },
          }),
        );
      } else {
        await unwrap(
          client.POST("/api/v1/workspaces/{ws}/forwarding/rules", {
            params: { path: { ws: workspaceId } },
            body: { ...body, destination_id: destinationId },
          }),
        );
      }
      onSaved();
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  const ready = name.trim() !== "" && name.trim().length <= MAX_NAME_CHARS && destinationId !== "" && eventName.trim() !== "" && rows.length > 0;

  return (
    <Dialog title={existing ? `Change ${existing.name}` : "New forwarding rule"} onClose={onClose} wide>
      <form
        className="stack"
        onSubmit={(event) => {
          event.preventDefault();
          void save();
        }}
      >
        <TextField label="Name" value={name} onChange={(event) => setName(event.target.value)} maxLength={MAX_NAME_CHARS} autoComplete="off" />
        <SelectField label="Destination" value={destinationId} onChange={(event) => setDestinationId(event.target.value)}>
          <option value="">Choose a destination</option>
          {destinations.map((destination) => (
            <option key={destination.id} value={destination.id}>
              {destination.name}
            </option>
          ))}
        </SelectField>
        {events.status === "ready" ? (
          <SelectField
            label="Event"
            value={eventName}
            onChange={(event) => {
              setEventName(event.target.value);
              setFilter([]);
            }}
            hint="The exact name of the events the rule selects."
          >
            <option value="">Choose an event</option>
            {eventName !== "" && !events.data.some((entry) => entry.name === eventName) ? <option value={eventName}>{eventName}</option> : null}
            {events.data.map((entry) => (
              <option key={entry.name} value={entry.name}>
                {entry.name}
              </option>
            ))}
          </SelectField>
        ) : (
          <TextField
            label="Event"
            value={eventName}
            onChange={(event) => setEventName(event.target.value)}
            hint="The exact name of the events the rule selects."
            autoComplete="off"
          />
        )}
        <label className="check">
          <input type="checkbox" checked={active} onChange={(event) => setActive(event.target.checked)} />
          <span>Active</span>
        </label>
        {eventName.trim() !== "" ? (
          <PropertyFilterEditor workspaceId={workspaceId} eventName={eventName.trim()} value={filter} onChange={setFilter} />
        ) : (
          <p className="faint">Choose an event to filter on its properties.</p>
        )}
        <fieldset className="condition-group">
          <legend className="field-label">Payload mapping</legend>
          <p className="faint">Each output key of the delivered JSON reads one source.</p>
          {rows.map((row, at) => {
            const label = `row ${at + 1}`;
            return (
              <div key={at} className="filter-row" role="group" aria-label={`Mapping ${label}`}>
                <TextField
                  label={`Output key of ${label}`}
                  value={row.key}
                  onChange={(event) => change(at, { key: event.target.value })}
                  error={rowErrors[at]}
                  autoComplete="off"
                />
                <SelectField
                  label={`Source of ${label}`}
                  value={row.source}
                  onChange={(event) => change(at, { source: event.target.value as SourceId })}
                >
                  {SOURCES.map((source) => (
                    <option key={source.id} value={source.id}>
                      {source.label}
                    </option>
                  ))}
                </SelectField>
                {row.source === "event.properties" ? (
                  <TextField
                    label={`Property pointer of ${label}`}
                    value={row.pointer}
                    onChange={(event) => change(at, { pointer: event.target.value })}
                    hint="Such as /order/total. A missing property is null."
                    autoComplete="off"
                    mono
                  />
                ) : null}
                {row.source === "value" ? (
                  <>
                    <SelectField
                      label={`Value type of ${label}`}
                      value={row.valueType}
                      onChange={(event) => change(at, { valueType: event.target.value as ValueType, value: event.target.value === "boolean" ? "true" : "" })}
                    >
                      <option value="string">Text</option>
                      <option value="number">Number</option>
                      <option value="boolean">Yes or no</option>
                    </SelectField>
                    {row.valueType === "boolean" ? (
                      <SelectField label={`Value of ${label}`} value={row.value || "true"} onChange={(event) => change(at, { value: event.target.value })}>
                        <option value="true">Yes</option>
                        <option value="false">No</option>
                      </SelectField>
                    ) : (
                      <TextField
                        label={`Value of ${label}`}
                        type={row.valueType === "number" ? "number" : "text"}
                        value={row.value}
                        onChange={(event) => change(at, { value: event.target.value })}
                        autoComplete="off"
                      />
                    )}
                  </>
                ) : null}
                <Button onClick={() => setRows(rows.filter((_, index) => index !== at))}>
                  Remove <span className="visually-hidden">{label}</span>
                </Button>
              </div>
            );
          })}
          <Button onClick={() => setRows([...rows, newRow()])}>Add a mapping</Button>
        </fieldset>
        {failure ? <InlineError title={failure.title} detail={failure.detail} traceId={failure.traceId} /> : null}
        <div className="dialog-actions dialog-actions-wrap">
          <Button onClick={onClose}>Cancel</Button>
          <Button type="submit" variant="primary" disabled={!ready || busy}>
            {existing ? "Save the rule" : "Create the rule"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}

/** The destinations a forwarding rule may name. */
export function forwardingTargets(destinations: Destination[], kinds: readonly DestinationKind[]): Destination[] {
  return destinations.filter((destination) => kindOf(kinds, destination.kind)?.forwards === true);
}
