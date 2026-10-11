import { Button, SelectField, TextField } from "../../ui";
import { FILTER_OPS, operatorsFor } from "./labels";
import type { SchemaProperty } from "./labels";

export interface FilterRow {
  id: number;
  key: string;
  op: string;
  value: string;
}

export interface BuiltFilter {
  key: string;
  op: string;
  value?: unknown;
}

/** The declared type of `key` in the schema, or null when it is not declared. */
export function typeOf(properties: readonly SchemaProperty[], key: string): string | null {
  return properties.find((property) => property.name === key)?.type ?? null;
}

function words(text: string): string[] {
  return text
    .split(",")
    .map((item) => item.trim())
    .filter((item) => item !== "");
}

/** Types one row by its property's type; a message when it cannot be sent. */
export function buildFilter(row: FilterRow, type: string | null): BuiltFilter | string {
  const key = row.key.trim();
  if (key === "") return "Name the property for each filter.";
  const allowed = operatorsFor(type);
  // The operator the row shows: the first the type takes when the chosen one does not apply.
  const op = allowed.includes(row.op) ? row.op : (allowed[0] ?? "eq");
  if (op === "exists") {
    return { key, op: "exists", value: row.value !== "absent" };
  }
  const text = row.value.trim();
  if (text === "") return `Enter a value for "${key}".`;
  const numeric = type === "number" || type === "integer";
  const parse = (item: string): unknown => {
    if (numeric) return Number(item);
    if (type === "boolean") return item === "true";
    return item;
  };
  if (op === "in") {
    const items = words(text);
    if (items.length === 0) return `Enter at least one value for "${key}".`;
    if (numeric && items.some((item) => Number.isNaN(Number(item)))) return `"${key}" takes numbers.`;
    return { key, op: "in", value: items.map(parse) };
  }
  if (numeric && Number.isNaN(Number(text))) return `"${key}" takes a number.`;
  return { key, op, value: parse(text) };
}

interface FilterBuilderProps {
  properties: readonly SchemaProperty[];
  rows: readonly FilterRow[];
  onChange: (rows: FilterRow[]) => void;
}

export function FilterBuilder({ properties, rows, onChange }: FilterBuilderProps) {
  function update(id: number, change: Partial<FilterRow>) {
    onChange(rows.map((row) => (row.id === id ? { ...row, ...change } : row)));
  }
  const nextId = rows.reduce((largest, row) => Math.max(largest, row.id), 0) + 1;

  return (
    <fieldset className="filter-builder">
      <legend className="field-label">Property filters</legend>
      <datalist id="explorer-properties">
        {properties.map((property) => (
          <option key={property.name} value={property.name} />
        ))}
      </datalist>
      {rows.length === 0 ? <p className="faint">Every event in the range matches.</p> : null}
      {rows.map((row, index) => {
        const type = typeOf(properties, row.key.trim());
        const allowed = operatorsFor(type);
        const shown = allowed.includes(row.op) ? row.op : (allowed[0] ?? "eq");
        const label = `filter ${index + 1}`;
        return (
          <div key={row.id} className="filter-row" role="group" aria-label={`Filter ${index + 1}`}>
            <TextField
              label={`Property of ${label}`}
              list="explorer-properties"
              value={row.key}
              onChange={(event) => update(row.id, { key: event.target.value })}
              autoComplete="off"
              hint={type === null ? "Not in the schema: compared as text" : undefined}
            />
            <SelectField
              label={`Operator of ${label}`}
              value={shown}
              onChange={(event) => update(row.id, { op: event.target.value })}
            >
              {FILTER_OPS.filter((op) => allowed.includes(op.value)).map((op) => (
                <option key={op.value} value={op.value}>
                  {op.label}
                </option>
              ))}
            </SelectField>
            {shown === "exists" ? (
              <SelectField label={`Presence of ${label}`} value={row.value === "absent" ? "absent" : "present"} onChange={(event) => update(row.id, { value: event.target.value })}>
                <option value="present">Present</option>
                <option value="absent">Absent</option>
              </SelectField>
            ) : type === "boolean" ? (
              <SelectField label={`Value of ${label}`} value={row.value === "false" ? "false" : "true"} onChange={(event) => update(row.id, { value: event.target.value })}>
                <option value="true">Yes</option>
                <option value="false">No</option>
              </SelectField>
            ) : (
              <TextField
                label={`Value of ${label}`}
                value={row.value}
                onChange={(event) => update(row.id, { value: event.target.value })}
                autoComplete="off"
                hint={shown === "in" ? "Separate values with commas" : type === "timestamp" ? "For example 2026-10-01T09:00:00Z" : undefined}
              />
            )}
            <Button onClick={() => onChange(rows.filter((other) => other.id !== row.id))}>
              Remove <span className="visually-hidden">{label}</span>
            </Button>
          </div>
        );
      })}
      <Button onClick={() => onChange([...rows, { id: nextId, key: "", op: "eq", value: "" }])}>Add a filter</Button>
    </fieldset>
  );
}
