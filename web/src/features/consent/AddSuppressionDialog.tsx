import { useState, type SyntheticEvent } from "react";

import { client, describeError, unwrap, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError, SelectField, TextField } from "../../ui";
import { KINDS } from "./labels";

interface AddSuppressionDialogProps {
  workspaceId: string;
  onClose: () => void;
  onAdded: (message: string) => void;
}

export function AddSuppressionDialog({ workspaceId, onClose, onAdded }: AddSuppressionDialogProps) {
  const [kind, setKind] = useState("email");
  const [value, setValue] = useState("");
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      const result = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/suppressions", {
          params: { path: { ws: workspaceId } },
          body: { kind, value: value.trim(), note: note.trim() === "" ? null : note.trim() },
        }),
      );
      onAdded(result.added ? "Suppressed. Nothing is sent to it any more." : "It was already suppressed.");
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title="Suppress an address" onClose={onClose}>
      <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
        <p className="muted">
          Messages are never sent to a suppressed address or number, whatever the person has chosen. Only a one-way hash is
          stored; the value itself is not kept.
        </p>
        <SelectField label="Kind" value={kind} onChange={(event) => setKind(event.target.value)}>
          {KINDS.map((option) => (
            <option key={option.value} value={option.value}>
              {option.label}
            </option>
          ))}
        </SelectField>
        <TextField
          label="Address or number"
          value={value}
          onChange={(event) => setValue(event.target.value)}
          autoComplete="off"
          required
        />
        <TextField
          label="Note"
          hint="Optional, up to 500 characters. Do not write the address or number here."
          value={note}
          onChange={(event) => setNote(event.target.value)}
          maxLength={500}
          autoComplete="off"
        />
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button type="submit" variant="primary" disabled={busy || value.trim() === ""}>
            {busy ? "Suppressing" : "Suppress"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
