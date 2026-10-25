import { useState } from "react";

import { TextField } from "../../../ui";

interface Props {
  label: string;
  hint: string;
  min: number;
  max: number;
  value: number;
  readOnly: boolean;
  onValue: (next: number) => void;
}

/**
 * A whole-number field that keeps what is typed while it is edited. An emptied or half-typed field
 * changes nothing in the document, so a value the person never typed (0) is never sent.
 */
export function NumberField({ label, hint, min, max, value, readOnly, onValue }: Props) {
  const [raw, setRaw] = useState(String(value));
  // The document changed from outside (another block, an undo): show it.
  const [seen, setSeen] = useState(value);
  if (seen !== value) {
    setSeen(value);
    if (raw.trim() === "" || Number(raw) !== value) setRaw(String(value));
  }
  const empty = raw.trim() === "";
  return (
    <TextField
      label={label}
      hint={hint}
      error={empty ? `Enter a number from ${min} to ${max}.` : undefined}
      type="number"
      min={min}
      max={max}
      value={raw}
      readOnly={readOnly}
      onChange={(event) => {
        const next = event.target.value;
        setRaw(next);
        if (/^\d+$/.test(next.trim())) {
          const number = Number(next);
          setSeen(number);
          onValue(number);
        }
      }}
    />
  );
}
