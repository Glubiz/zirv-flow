import { useState, type SyntheticEvent } from "react";

import { client, describeError, unwrap, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError, TextField } from "../../ui";
import { FieldInputs } from "./FieldInputs";
import {
  configFrom,
  credentialFrom,
  filled,
  initialValues,
  shapesValid,
  type Source,
  type SourceKind,
  type Values,
} from "./labels";

interface EditSourceDialogProps {
  workspaceId: string;
  source: Source;
  kind: SourceKind;
  onClose: () => void;
  onSaved: (source: Source) => void;
}

/**
 * Changes a source. The credential is never shown: a person who wants a new one turns on
 * "Replace the credential" and types every credential field again.
 */
export function EditSourceDialog({ workspaceId, source, kind, onClose, onSaved }: EditSourceDialogProps) {
  const [name, setName] = useState(source.name);
  const [enabled, setEnabled] = useState(source.enabled);
  const [schedule, setSchedule] = useState(source.schedule ?? "");
  const [values, setValues] = useState<Values>(() =>
    initialValues([...kind.config_fields, ...kind.credential_fields], source.config),
  );
  const [replacing, setReplacing] = useState(false);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  const change = (key: string, value: string) => setValues((current) => ({ ...current, [key]: value }));
  const valid =
    name.trim() !== "" &&
    filled(kind.config_fields, values) &&
    shapesValid(kind.config_fields, values) &&
    (!replacing || filled(kind.credential_fields, values));

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      const saved = await unwrap(
        client.PATCH("/api/v1/workspaces/{ws}/sources/{id}", {
          params: { path: { ws: workspaceId, id: source.id } },
          body: {
            name: name.trim(),
            config: configFrom(kind.config_fields, values),
            enabled,
            schedule: schedule.trim(),
            ...(replacing ? { credential: credentialFrom(kind.credential_fields, values) } : {}),
          },
        }),
      );
      onSaved(saved);
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title={`Edit ${source.name}`} onClose={onClose}>
      <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
        <TextField
          label="Name"
          value={name}
          onChange={(event) => setName(event.target.value)}
          maxLength={128}
          autoComplete="off"
        />
        <FieldInputs
          fields={kind.config_fields}
          values={values}
          onChange={change}
          disabled={busy}
          kind={kind.kind}
          workspaceId={workspaceId}
          sourceId={source.id}
        />
        <TextField
          label="Schedule (optional)"
          hint="A cron expression in UTC, such as 0 * * * *. Leave empty for none."
          value={schedule}
          onChange={(event) => setSchedule(event.target.value)}
          mono
        />
        <label className="check">
          <input type="checkbox" checked={enabled} onChange={(event) => setEnabled(event.target.checked)} />
          <span>Enabled</span>
        </label>
        {kind.credential_fields.length > 0 ? (
          <fieldset className="stack">
            <legend className="strong">Credential</legend>
            <label className="check">
              <input type="checkbox" checked={replacing} onChange={(event) => setReplacing(event.target.checked)} />
              <span>Replace the credential</span>
            </label>
            {replacing ? (
              <FieldInputs fields={kind.credential_fields} values={values} onChange={change} disabled={busy} />
            ) : (
              <p className="muted">The stored credential stays as it is.</p>
            )}
          </fieldset>
        ) : null}
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button type="submit" variant="primary" disabled={busy || !valid}>
            {busy ? "Saving" : "Save changes"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
