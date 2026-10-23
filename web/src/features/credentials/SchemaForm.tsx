import { SelectField, TextField } from "../../ui";
import { fieldsOf, isRequired, type Schema, type Values } from "./labels";

interface SchemaFormProps {
  schema: Schema;
  group: "secret" | "public";
  values: Values;
  onChange: (key: string, value: string) => void;
  disabled?: boolean;
}

/**
 * The form of one group of a credential schema, drawn from data. A secret field is a password
 * input that is never filled in: the server does not return secrets, so there is nothing to show.
 */
export function SchemaForm({ schema, group, values, onChange, disabled = false }: SchemaFormProps) {
  return (
    <>
      {fieldsOf(schema, group).map((field) => {
        const value = values[field.key] ?? "";
        const label = isRequired(field) ? field.label : `${field.label} (optional)`;
        if (field.type === "enum") {
          return (
            <SelectField
              key={field.key}
              label={label}
              value={value}
              disabled={disabled}
              onChange={(event) => onChange(field.key, event.target.value)}
            >
              <option value="">{isRequired(field) ? "Choose one" : "Not set"}</option>
              {(field.options ?? []).map((option) => (
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
            type={group === "secret" ? "password" : field.type === "integer" ? "number" : field.type === "url" ? "url" : "text"}
            value={value}
            disabled={disabled}
            autoComplete={group === "secret" ? "new-password" : "off"}
            onChange={(event) => onChange(field.key, event.target.value)}
          />
        );
      })}
    </>
  );
}
