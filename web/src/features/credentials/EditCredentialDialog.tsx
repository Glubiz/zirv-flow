import { useState, type SyntheticEvent } from "react";

import { client, describeError, unwrap, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError, TextField } from "../../ui";
import { complete, emptyValues, publicFrom, type Credential, type Values } from "./labels";
import { SchemaForm } from "./SchemaForm";

interface EditCredentialDialogProps {
  workspaceId: string;
  credential: Credential;
  onClose: () => void;
  onSaved: () => void;
}

/**
 * Changes the name and the non-secret values of a credential. Secret values are not here: they
 * are replaced only through "Rotate", which asks for every secret field again.
 */
export function EditCredentialDialog({ workspaceId, credential, onClose, onSaved }: EditCredentialDialogProps) {
  const [name, setName] = useState(credential.name);
  const [values, setValues] = useState<Values>(() => emptyValues(credential.schema, "public", credential.fields));
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const hasFields = credential.schema.fields.some((field) => field.secret !== true);
  const valid = name.trim() !== "" && complete(credential.schema, "public", values);

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      await unwrap(
        client.PATCH("/api/v1/workspaces/{ws}/credentials/{id}", {
          params: { path: { ws: workspaceId, id: credential.id } },
          body: {
            name: name.trim(),
            ...(hasFields ? { fields: publicFrom(credential.schema, values) } : {}),
          },
        }),
      );
      onSaved();
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title={`Edit ${credential.name}`} onClose={onClose}>
      <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
        <TextField
          label="Name"
          value={name}
          onChange={(event) => setName(event.target.value)}
          maxLength={128}
          autoComplete="off"
          disabled={busy}
        />
        <SchemaForm
          schema={credential.schema}
          group="public"
          values={values}
          onChange={(key, value) => setValues((current) => ({ ...current, [key]: value }))}
          disabled={busy}
        />
        <p className="muted">The secret values stay as they are. Use Rotate to replace them.</p>
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
