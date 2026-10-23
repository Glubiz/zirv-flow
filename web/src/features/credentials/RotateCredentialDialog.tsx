import { useState, type SyntheticEvent } from "react";

import { client, describeError, unwrap, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError } from "../../ui";
import { complete, emptyValues, secretFrom, type Credential, type Values } from "./labels";
import { SchemaForm } from "./SchemaForm";

interface RotateCredentialDialogProps {
  workspaceId: string;
  credential: Credential;
  onClose: () => void;
  onRotated: () => void;
}

/**
 * Replaces the secret values of a credential. Every secret field starts empty and must be typed
 * again; every source and destination that uses the credential follows on its next call.
 */
export function RotateCredentialDialog({ workspaceId, credential, onClose, onRotated }: RotateCredentialDialogProps) {
  const [values, setValues] = useState<Values>(() => emptyValues(credential.schema, "secret"));
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      await unwrap(
        client.PUT("/api/v1/workspaces/{ws}/credentials/{id}/secret", {
          params: { path: { ws: workspaceId, id: credential.id } },
          body: { secret: secretFrom(credential.schema, values) },
        }),
      );
      setValues({});
      onRotated();
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  const users = credential.used_by.length;
  return (
    <Dialog title={`Rotate ${credential.name}`} onClose={onClose}>
      <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
        <p className="muted">
          Type every secret value again. The old values are deleted.{" "}
          {users > 0
            ? `${users} ${users === 1 ? "source or destination uses" : "sources and destinations use"} this credential and will use the new values from their next call.`
            : "Nothing uses this credential yet."}
        </p>
        <SchemaForm
          schema={credential.schema}
          group="secret"
          values={values}
          onChange={(key, value) => setValues((current) => ({ ...current, [key]: value }))}
          disabled={busy}
        />
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button type="submit" variant="primary" disabled={busy || !complete(credential.schema, "secret", values)}>
            {busy ? "Rotating" : "Rotate secret"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
