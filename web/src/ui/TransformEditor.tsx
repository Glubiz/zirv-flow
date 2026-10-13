import { useId } from "react";

import { Button } from "./Button";
import {
  newTransform,
  opLabel,
  TRAIT_TYPES,
  TRANSFORM_OPS,
  type TraitType,
  type TransformDraft,
  type TransformOp,
} from "./transforms";

const TYPE_LABELS: Record<TraitType, string> = {
  string: "Text",
  number: "Number",
  boolean: "Yes or no",
  timestamp: "Date and time",
  date: "Date",
  string_list: "List of text",
};

const FORMATS = [
  { value: "%Y-%m-%d", label: "2026-03-31" },
  { value: "%d/%m/%Y", label: "31/03/2026" },
  { value: "%m/%d/%Y", label: "03/31/2026" },
  { value: "%d.%m.%Y", label: "31.03.2026" },
  { value: "%Y-%m-%d %H:%M:%S", label: "2026-03-31 14:30:00" },
  { value: "unix", label: "Unix seconds" },
  { value: "unix_ms", label: "Unix milliseconds" },
];

interface TransformEditorProps {
  column: string;
  /** The other columns of the file, which a join can take values from. */
  others: readonly string[];
  steps: readonly TransformDraft[];
  disabled: boolean;
  onChange: (steps: TransformDraft[]) => void;
}

/** The steps a cell goes through before it reaches its target, in order. */
export function TransformEditor({ column, others, steps, disabled, onChange }: TransformEditorProps) {
  const formats = useId();

  function update(index: number, change: Partial<TransformDraft>) {
    onChange(steps.map((step, at) => (at === index ? { ...step, ...change } : step)));
  }

  return (
    <div className="stack-tight">
      {steps.length === 0 ? <p className="faint">The cell is used as it is.</p> : null}
      <ol className="steps">
        {steps.map((step, index) => {
          const label = `step ${index + 1} of ${column}`;
          return (
            <li key={step.id} className="step">
              <div className="row">
                <select
                  className="select"
                  aria-label={`Type of ${label}`}
                  value={step.op}
                  disabled={disabled}
                  onChange={(event) => {
                    const fresh = newTransform(event.target.value as TransformOp);
                    update(index, { ...fresh, id: step.id });
                  }}
                >
                  {TRANSFORM_OPS.map((op) => (
                    <option key={op} value={op}>
                      {opLabel(op)}
                    </option>
                  ))}
                </select>
                <Button
                  onClick={() => onChange(steps.filter((_, at) => at !== index))}
                  disabled={disabled}
                  aria-label={`Remove ${label}`}
                >
                  Remove
                </Button>
              </div>
              {step.op === "cast" ? (
                <select
                  className="select"
                  aria-label={`Type to convert to in ${label}`}
                  value={step.to}
                  disabled={disabled}
                  onChange={(event) => update(index, { to: event.target.value as TraitType })}
                >
                  {TRAIT_TYPES.map((type) => (
                    <option key={type} value={type}>
                      {TYPE_LABELS[type]}
                    </option>
                  ))}
                </select>
              ) : null}
              {step.op === "timestamp" ? (
                <>
                  <input
                    className="input input-mono"
                    aria-label={`Format of ${label}`}
                    list={formats}
                    value={step.format}
                    disabled={disabled}
                    onChange={(event) => update(index, { format: event.target.value })}
                  />
                  <datalist id={formats}>
                    {FORMATS.map((format) => (
                      <option key={format.value} value={format.value}>
                        {format.label}
                      </option>
                    ))}
                  </datalist>
                  <span className="field-hint">
                    A pattern such as %d/%m/%Y %H:%M, or unix. Without a time zone the time is read as UTC.
                  </span>
                </>
              ) : null}
              {step.op === "concat" ? (
                <>
                  <fieldset className="checks">
                    <legend>Join with</legend>
                    {others.map((other) => (
                      <label key={other} className="check">
                        <input
                          type="checkbox"
                          checked={step.columns.includes(other)}
                          disabled={disabled}
                          onChange={(event) =>
                            update(index, {
                              columns: event.target.checked
                                ? [...step.columns, other]
                                : step.columns.filter((name) => name !== other),
                            })
                          }
                        />
                        <span>{other}</span>
                      </label>
                    ))}
                  </fieldset>
                  <input
                    className="input"
                    aria-label={`Separator of ${label}`}
                    value={step.separator}
                    disabled={disabled}
                    onChange={(event) => update(index, { separator: event.target.value })}
                  />
                </>
              ) : null}
              {step.op === "constant" ? (
                <input
                  className="input"
                  aria-label={`Fixed value of ${label}`}
                  value={step.value}
                  disabled={disabled}
                  onChange={(event) => update(index, { value: event.target.value })}
                />
              ) : null}
              {step.op === "value_map" ? (
                <>
                  <textarea
                    className="input textarea input-mono"
                    aria-label={`Replacements of ${label}`}
                    value={step.entries}
                    placeholder={"M=male\nF=female"}
                    disabled={disabled}
                    onChange={(event) => update(index, { entries: event.target.value })}
                  />
                  <span className="field-hint">One replacement per line, written old=new. Other values stay as they are.</span>
                </>
              ) : null}
              {step.op === "split" ? (
                <input
                  className="input"
                  aria-label={`Separator of ${label}`}
                  value={step.separator}
                  disabled={disabled}
                  onChange={(event) => update(index, { separator: event.target.value })}
                />
              ) : null}
            </li>
          );
        })}
      </ol>
      <select
        className="select"
        aria-label={`Add a step to ${column}`}
        value=""
        disabled={disabled || steps.length >= 10}
        onChange={(event) => {
          if (event.target.value !== "") {
            onChange([...steps, newTransform(event.target.value as TransformOp)]);
          }
        }}
      >
        <option value="">Add a step…</option>
        {TRANSFORM_OPS.map((op) => (
          <option key={op} value={op}>
            {opLabel(op)}
          </option>
        ))}
      </select>
    </div>
  );
}
