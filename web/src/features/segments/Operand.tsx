import { useState } from "react";

import { Button, SelectField, TextField } from "../../ui";
import { DURATION_PRESETS, operandType, shapeOf, type Op, type ValueType } from "./operators";

/** What a list or range item holds before it is filled in. */
function emptyItem(type: ValueType): unknown {
  return type === "string" || type === "timestamp" || type === "date" ? "" : null;
}

/** The operand a fresh choice of operator starts with. */
export function initialOperand(type: ValueType, op: Op): unknown {
  const item = emptyItem(operandType(type));
  switch (shapeOf(type, op)) {
    case "list":
      return [item];
    case "pair":
      return [item, item];
    default:
      return undefined;
  }
}

function toLocalInput(value: unknown): string {
  return typeof value === "string" && /^\d{4}-\d\d-\d\dT\d\d:\d\d/.test(value) ? value.slice(0, 16) : "";
}

function parseNumber(text: string): number | undefined {
  if (text.trim() === "") return undefined;
  const parsed = Number(text);
  return Number.isFinite(parsed) ? parsed : undefined;
}

interface ScalarProps {
  label: string;
  type: ValueType;
  value: unknown;
  onChange: (value: unknown) => void;
  disabled: boolean;
  error?: string;
  hint?: string;
}

/** One value of a given type: text, a number, yes or no, or a date or time picker. */
function ScalarInput({ label, type, value, onChange, disabled, error, hint }: ScalarProps) {
  switch (type) {
    case "boolean":
      return (
        <SelectField
          label={label}
          value={typeof value === "boolean" ? String(value) : ""}
          onChange={(event) => onChange(event.target.value === "" ? undefined : event.target.value === "true")}
          disabled={disabled}
          error={error}
          hint={hint}
        >
          <option value="">Choose</option>
          <option value="true">Yes</option>
          <option value="false">No</option>
        </SelectField>
      );
    case "number":
    case "integer":
      return (
        <NumberText
          label={label}
          value={value}
          onChange={onChange}
          disabled={disabled}
          error={error}
          hint={hint ?? (type === "integer" ? "A whole number" : undefined)}
        />
      );
    case "timestamp":
      return (
        <TextField
          label={label}
          type="datetime-local"
          value={toLocalInput(value)}
          onChange={(event) => onChange(event.target.value === "" ? "" : `${event.target.value}:00Z`)}
          disabled={disabled}
          error={error}
          hint={hint ?? "In UTC"}
        />
      );
    case "date":
      return (
        <TextField
          label={label}
          type="date"
          value={typeof value === "string" ? value : ""}
          onChange={(event) => onChange(event.target.value)}
          disabled={disabled}
          error={error}
          hint={hint}
        />
      );
    default:
      return (
        <TextField
          label={label}
          value={typeof value === "string" ? value : ""}
          onChange={(event) => onChange(event.target.value)}
          disabled={disabled}
          error={error}
          hint={hint}
          autoComplete="off"
        />
      );
  }
}

/** A number typed as text, so a half-typed "1." or "-" survives until it is a number. */
function NumberText({ label, value, onChange, disabled, error, hint }: Omit<ScalarProps, "type">) {
  const [text, setText] = useState(typeof value === "number" ? String(value) : "");
  const current = typeof value === "number" ? value : undefined;
  const shown = parseNumber(text) === current ? text : current === undefined ? "" : String(current);
  return (
    <TextField
      label={label}
      inputMode="decimal"
      value={shown}
      onChange={(event) => {
        setText(event.target.value);
        const parsed = parseNumber(event.target.value);
        onChange(parsed);
      }}
      disabled={disabled}
      error={error}
      hint={hint}
      autoComplete="off"
    />
  );
}

interface OperandProps {
  /** Names the control, such as "Value of condition 1". */
  label: string;
  type: ValueType;
  op: Op;
  value: unknown;
  onChange: (value: unknown) => void;
  disabled: boolean;
  error?: string;
}

/** The value of a comparison, shaped by the type and the operator. Nothing for a presence operator. */
export function Operand({ label, type, op, value, onChange, disabled, error }: OperandProps) {
  const shape = shapeOf(type, op);
  const item = operandType(type);
  const items = Array.isArray(value) ? (value as unknown[]) : [];

  if (shape === "none") return null;
  if (shape === "scalar") {
    return <ScalarInput label={label} type={item} value={value} onChange={onChange} disabled={disabled} error={error} />;
  }
  if (shape === "duration") {
    return (
      <div className="operand">
        <TextField
          label={label}
          value={typeof value === "string" ? value : ""}
          onChange={(event) => onChange(event.target.value)}
          disabled={disabled}
          error={error}
          hint="A duration in days or hours, such as P7D or PT12H"
          autoComplete="off"
          mono
        />
        {disabled ? null : (
          <div className="row">
            {DURATION_PRESETS.map((preset) => (
              <Button key={preset.value} onClick={() => onChange(preset.value)} aria-label={`Set ${label} to ${preset.label}`}>
                {preset.label}
              </Button>
            ))}
          </div>
        )}
      </div>
    );
  }
  if (shape === "pair") {
    const names = ["from", "to"];
    return (
      <fieldset className="operand">
        <legend className="field-label">{label}</legend>
        {names.map((name, at) => (
          <ScalarInput
            key={name}
            label={`${label}, ${name}`}
            type={item}
            value={items[at]}
            onChange={(next) => onChange(names.map((_, index) => (index === at ? next : (items[index] ?? emptyItem(item)))))}
            disabled={disabled}
            error={at === 0 ? error : undefined}
          />
        ))}
      </fieldset>
    );
  }
  const shownItems = items.length > 0 ? items : [emptyItem(item)];
  return (
    <fieldset className="operand">
      <legend className="field-label">{label}</legend>
      {shownItems.map((entry, at) => (
        <div className="operand-item" key={at}>
          <ScalarInput
            label={`${label}, value ${at + 1}`}
            type={item}
            value={entry}
            onChange={(next) => onChange(shownItems.map((existing, index) => (index === at ? next : existing)))}
            disabled={disabled}
            error={at === 0 ? error : undefined}
          />
          {disabled || shownItems.length === 1 ? null : (
            <Button onClick={() => onChange(shownItems.filter((_, index) => index !== at))}>
              Remove <span className="visually-hidden">{`${label}, value ${at + 1}`}</span>
            </Button>
          )}
        </div>
      ))}
      {disabled ? null : (
        <Button onClick={() => onChange([...shownItems, emptyItem(item)])} aria-label={`Add a value to ${label}`}>
          Add a value
        </Button>
      )}
    </fieldset>
  );
}
