import { useRef, useState, type SyntheticEvent } from "react";

import { client, describeError, unwrap, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError, SelectField, TextField } from "../../ui";
import { formatWindow, toDays, wholeMonths, type WindowUnit } from "./retentionUnits";

export interface EditableWindow {
  key: "messages" | "decision_log";
  label: string;
  value: number;
  min: number | null;
  max: number | null;
}

function bounds(window: EditableWindow): string {
  const { min, max } = window;
  if (min !== null && max !== null) return `${formatWindow(min)} to ${formatWindow(max)}`;
  if (min !== null) return `At least ${formatWindow(min)}`;
  if (max !== null) return `Up to ${formatWindow(max)}`;
  return "";
}

/** The check that runs before anything is sent: a whole, positive number of days within the bounds. */
function problemWith(window: EditableWindow, amount: string, unit: WindowUnit): string | null {
  const number = Number(amount);
  if (amount.trim() === "" || !Number.isSafeInteger(number) || number < 1) {
    return "Enter a whole number of 1 or more.";
  }
  const days = toDays(number, unit);
  if ((window.min !== null && days < window.min) || (window.max !== null && days > window.max)) {
    return `Out of range. Choose ${bounds(window)}.`;
  }
  return null;
}

export function ShortenWindowDialog({
  workspaceId,
  window,
  onClose,
  onSaved,
}: {
  workspaceId: string;
  window: EditableWindow;
  onClose: () => void;
  onSaved: () => void;
}) {
  const months = wholeMonths(window.value);
  const [amount, setAmount] = useState(String(months ?? window.value));
  const [unit, setUnit] = useState<WindowUnit>(months === null ? "days" : "months");
  const [invalid, setInvalid] = useState<string | null>(null);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [busy, setBusy] = useState(false);
  const sending = useRef(false);

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    if (sending.current) return;
    setFailure(null);
    const problem = problemWith(window, amount, unit);
    setInvalid(problem);
    if (problem) return;
    const days = toDays(Number(amount), unit);
    sending.current = true;
    setBusy(true);
    try {
      await unwrap(
        client.PUT("/api/v1/workspaces/{ws}/retention", {
          params: { path: { ws: workspaceId } },
          body: window.key === "messages" ? { messages_days: days } : { decision_days: days },
        }),
      );
      onSaved();
    } catch (error) {
      setFailure(describeError(error));
      sending.current = false;
      setBusy(false);
    }
  }

  return (
    <Dialog title="Shorten a window" onClose={onClose}>
      <p className="muted">{window.label}</p>
      <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
        <div className="window-input">
          <TextField
            label="Keep for"
            type="number"
            inputMode="numeric"
            min={1}
            value={amount}
            onChange={(event) => setAmount(event.target.value)}
            hint={bounds(window)}
            error={invalid ?? undefined}
          />
          <SelectField label="Unit" value={unit} onChange={(event) => setUnit(event.target.value as WindowUnit)}>
            <option value="days">Days</option>
            <option value="months">Months</option>
          </SelectField>
        </div>
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button type="submit" variant="primary" disabled={busy}>
            {busy ? "Saving" : "Save"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
