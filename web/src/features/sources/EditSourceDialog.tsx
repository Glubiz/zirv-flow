import { useState, type SyntheticEvent } from "react";

import { client, describeError, unwrap, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError, TextField } from "../../ui";
import { CredentialPicker } from "../credentials";
import { FieldInputs } from "./FieldInputs";
import {
  configFrom,
  filled,
  hasManifest,
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
 * Changes a source. The credential is a workspace credential, chosen by id and never shown; its
 * secret values are replaced on the Credentials page, in one place.
 */
export function EditSourceDialog({ workspaceId, source, kind, onClose, onSaved }: EditSourceDialogProps) {
  const [name, setName] = useState(source.name);
  const [enabled, setEnabled] = useState(source.enabled);
  const [schedule, setSchedule] = useState(source.schedule ?? "");
  const [values, setValues] = useState<Values>(() =>
    initialValues(kind.config_fields, source.config),
  );
  const [credentialId, setCredentialId] = useState(source.credential_id ?? "");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  const change = (key: string, value: string) => setValues((current) => ({ ...current, [key]: value }));
  const valid =
    name.trim() !== "" &&
    filled(kind.config_fields, values) &&
    shapesValid(kind.config_fields, values) &&
    (credentialId !== "" || !kind.credential_fields.some((field) => field.required));

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
            ...(credentialId !== "" && credentialId !== source.credential_id ? { credential_id: credentialId } : {}),
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
            <CredentialPicker
              workspaceId={workspaceId}
              fields={kind.credential_fields}
              manifest={hasManifest(kind)}
              value={credentialId}
              onChange={setCredentialId}
              purpose={kind.label}
              disabled={busy}
            />
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
