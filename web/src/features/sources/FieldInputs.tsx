import { SelectField, TextField } from "../../ui";
import type { FieldSpec, Values } from "./labels";

interface FieldInputsProps {
  fields: FieldSpec[];
  values: Values;
  onChange: (key: string, value: string) => void;
  disabled?: boolean;
}

function inputType(field: FieldSpec): "text" | "url" | "number" | "password" {
  if (field.secret) return "password";
  return field.kind === "url" ? "url" : field.kind === "number" ? "number" : "text";
}

/**
 * The form of a source type, rendered from its field specs. A secret field is a password input
 * that is never filled in: the server does not return credentials, so there is nothing to show.
 */
export function FieldInputs({ fields, values, onChange, disabled = false }: FieldInputsProps) {
  return (
    <>
      {fields.map((field) => {
        const value = values[field.key] ?? "";
        const label = field.required ? field.label : `${field.label} (optional)`;
        if (field.kind === "bool") {
          return (
            <label className="check" key={field.key}>
              <input
                type="checkbox"
                checked={value === "true"}
                disabled={disabled}
                onChange={(event) => onChange(field.key, event.target.checked ? "true" : "false")}
              />
              <span>
                {field.label} {field.help ? <span className="faint">{field.help}</span> : null}
              </span>
            </label>
          );
        }
        if (field.kind === "select") {
          return (
            <SelectField
              key={field.key}
              label={label}
              hint={field.help || undefined}
              value={value}
              disabled={disabled}
              onChange={(event) => onChange(field.key, event.target.value)}
            >
              {field.required ? null : <option value="">Not set</option>}
              {field.options.map((option) => (
                <option key={option} value={option}>
                  {option}
                </option>
              ))}
            </SelectField>
          );
        }
        return (
          <TextField
            key={field.key}
            label={label}
            hint={field.help || undefined}
            type={inputType(field)}
            value={value}
            disabled={disabled}
            autoComplete={field.secret ? "new-password" : "off"}
            onChange={(event) => onChange(field.key, event.target.value)}
          />
        );
      })}
    </>
  );
}
