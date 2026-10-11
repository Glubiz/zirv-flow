import { Button, SelectField, TextField } from "../../ui";
import { PROPERTY_TYPES, splitList, typeLabel, type SchemaProperty } from "./labels";

/** A property as the editor holds it: the enum is edited as comma-separated text. */
export interface PropertyRow {
  name: string;
  type: string;
  required: boolean;
  pii: boolean;
  description: string;
  enumText: string;
}

export function rowFrom(property: SchemaProperty): PropertyRow {
  return {
    name: property.name,
    type: property.type,
    required: property.required ?? false,
    pii: property.pii ?? false,
    description: property.description ?? "",
    enumText: (property.enum ?? []).join(", "),
  };
}

export function rowToProperty(row: PropertyRow): SchemaProperty {
  const values = row.type === "string" ? splitList(row.enumText) : [];
  return {
    name: row.name.trim(),
    type: row.type,
    required: row.required,
    pii: row.pii,
    description: row.description,
    enum: values.length > 0 ? values : null,
  };
}

export function blankRow(): PropertyRow {
  return { name: "", type: "string", required: false, pii: false, description: "", enumText: "" };
}

interface PropertyEditorProps {
  rows: PropertyRow[];
  onChange: (rows: PropertyRow[]) => void;
}

export function PropertyEditor({ rows, onChange }: PropertyEditorProps) {
  function update(index: number, change: Partial<PropertyRow>) {
    onChange(rows.map((row, at) => (at === index ? { ...row, ...change } : row)));
  }

  return (
    <div className="stack">
      {rows.length === 0 ? <p className="muted">This draft has no properties yet.</p> : null}
      <ul className="prop-list list-plain">
        {rows.map((row, index) => (
          <li key={index} className="prop-row" role="group" aria-label={`Property ${index + 1}`}>
            <TextField
              label="Name"
              mono
              value={row.name}
              onChange={(event) => update(index, { name: event.target.value })}
              autoComplete="off"
            />
            <SelectField
              label="Type"
              value={row.type}
              onChange={(event) => update(index, { type: event.target.value })}
            >
              {PROPERTY_TYPES.map((option) => (
                <option key={option.value} value={option.value}>
                  {option.label}
                </option>
              ))}
            </SelectField>
            {row.type === "string" ? (
              <TextField
                label="Allowed values"
                hint="Optional. Separate with commas."
                value={row.enumText}
                onChange={(event) => update(index, { enumText: event.target.value })}
                autoComplete="off"
              />
            ) : null}
            <TextField
              label="Description"
              value={row.description}
              onChange={(event) => update(index, { description: event.target.value })}
              maxLength={500}
            />
            <div className="prop-flags">
              <label className="check">
                <input
                  type="checkbox"
                  checked={row.required}
                  onChange={(event) => update(index, { required: event.target.checked })}
                />
                <span>Required</span>
              </label>
              <label className="check">
                <input
                  type="checkbox"
                  checked={row.pii}
                  onChange={(event) => update(index, { pii: event.target.checked })}
                />
                <span>Personal data</span>
              </label>
              <Button
                onClick={() => onChange(rows.filter((_, at) => at !== index))}
                aria-label={`Remove property ${index + 1}`}
              >
                Remove
              </Button>
            </div>
          </li>
        ))}
      </ul>
      <div>
        <Button onClick={() => onChange([...rows, blankRow()])}>Add property</Button>
      </div>
    </div>
  );
}

export function PropertyTable({ properties, caption }: { properties: SchemaProperty[]; caption: string }) {
  if (properties.length === 0) {
    return <p className="muted">No properties.</p>;
  }
  return (
    <div className="table-scroll">
      <table className="table">
        <caption className="visually-hidden">{caption}</caption>
        <thead>
          <tr>
            <th scope="col">Property</th>
            <th scope="col">Type</th>
            <th scope="col">Rules</th>
          </tr>
        </thead>
        <tbody>
          {properties.map((property) => (
            <tr key={property.name}>
              <td className="strong mono">
                {property.name}
                {property.description ? <div className="faint">{property.description}</div> : null}
              </td>
              <td>{typeLabel(property.type)}</td>
              <td>
                {property.required ? <span className="pill-text">Required</span> : null}
                {property.pii ? <span className="pill-text">Personal data</span> : null}
                {property.enum && property.enum.length > 0 ? (
                  <div className="faint mono">{property.enum.join(" | ")}</div>
                ) : null}
                {!property.required && !property.pii && !(property.enum && property.enum.length > 0) ? "—" : null}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
