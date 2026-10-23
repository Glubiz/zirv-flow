import { useState, type SyntheticEvent } from "react";

import { client, describeError, unwrap, useResource, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError, LoadingState, SelectField, TextField } from "../../ui";
import {
  complete,
  emptyValues,
  publicFrom,
  secretFrom,
  type Credential,
  type Schema,
  type Values,
} from "./labels";
import { SchemaForm } from "./SchemaForm";

interface CreateCredentialDialogProps {
  workspaceId: string;
  onClose: () => void;
  onCreated: (credential: Credential) => void;
  /**
   * A schema to use instead of a built-in kind: the credential fields of the source or
   * destination being set up. The credential is then created as kind `custom`.
   */
  schema?: Schema;
  /** What the new credential is for, shown in the title and used as the first name suggestion. */
  purpose?: string;
}

/**
 * Creates a credential. The form is drawn from the schema of the chosen kind, which the server
 * lists; a secret input is never filled in and is emptied with the dialog.
 */
export function CreateCredentialDialog({ workspaceId, onClose, onCreated, schema, purpose }: CreateCredentialDialogProps) {
  const kinds = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/credential-kinds", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const [chosen, setChosen] = useState("");
  const [name, setName] = useState(purpose ? `${purpose} credential` : "");
  const [publicValues, setPublicValues] = useState<Values>({});
  const [secretValues, setSecretValues] = useState<Values>({});
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  const kindList = kinds.status === "ready" ? kinds.data : [];
  const active: Schema | null = schema ?? kindList.find((kind) => kind.id === chosen)?.schema ?? null;
  const kindId = schema ? "custom" : chosen;

  function choose(id: string) {
    setChosen(id);
    const next = kindList.find((kind) => kind.id === id)?.schema;
    setPublicValues(next ? emptyValues(next, "public") : {});
    setSecretValues(next ? emptyValues(next, "secret") : {});
  }

  const valid =
    name.trim() !== "" &&
    active !== null &&
    complete(active, "public", publicValues) &&
    complete(active, "secret", secretValues);

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    if (!active) return;
    setBusy(true);
    setFailure(null);
    try {
      const created = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/credentials", {
          params: { path: { ws: workspaceId } },
          body: {
            kind: kindId,
            name: name.trim(),
            ...(schema ? { schema } : {}),
            fields: publicFrom(active, publicValues),
            secret: secretFrom(active, secretValues),
          },
        }),
      );
      setSecretValues({});
      onCreated(created);
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title={purpose ? `New credential for ${purpose}` : "New credential"} onClose={onClose}>
      <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
        {schema ? null : kinds.status === "loading" ? (
          <LoadingState label="Loading credential kinds" />
        ) : kinds.status === "error" ? (
          <InlineError title="Could not load the credential kinds" detail={describeError(kinds.error).detail} />
        ) : (
          <SelectField label="Kind" value={chosen} onChange={(event) => choose(event.target.value)} disabled={busy}>
            <option value="">Choose a kind</option>
            {kindList.map((kind) => (
              <option key={kind.id} value={kind.id}>
                {kind.label}
              </option>
            ))}
          </SelectField>
        )}
        <TextField
          label="Name"
          hint="Shown wherever the credential is chosen. Two credentials cannot share a name."
          value={name}
          onChange={(event) => setName(event.target.value)}
          maxLength={128}
          autoComplete="off"
          disabled={busy}
        />
        {active ? (
          <>
            <SchemaForm
              schema={active}
              group="public"
              values={publicValues}
              onChange={(key, value) => setPublicValues((current) => ({ ...current, [key]: value }))}
              disabled={busy}
            />
            <SchemaForm
              schema={active}
              group="secret"
              values={secretValues}
              onChange={(key, value) => setSecretValues((current) => ({ ...current, [key]: value }))}
              disabled={busy}
            />
            <p className="muted">Secret values are stored encrypted. They cannot be read back, only replaced.</p>
          </>
        ) : null}
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button type="submit" variant="primary" disabled={busy || !valid}>
            {busy ? "Creating" : "Create credential"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
