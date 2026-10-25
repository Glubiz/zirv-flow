import { useState } from "react";

import { client, describeError, unwrap, useResource } from "../../api";
import { Button, InlineError, LoadingState, SelectField } from "../../ui";
import { CreateCredentialDialog } from "./CreateCredentialDialog";
import { covers, kindName, schemaFromHints, type Credential, type FieldHint } from "./labels";

interface CredentialPickerProps {
  workspaceId: string;
  /** The credential fields the source or destination type declares. */
  fields: FieldHint[];
  /** The type's settings hold a manifest, which may declare credential fields of its own. */
  manifest?: boolean;
  /** The id of the chosen credential, or empty. */
  value: string;
  onChange: (id: string) => void;
  /** What the credential is for, such as the source type's name. */
  purpose?: string;
  disabled?: boolean;
  /** Whether to offer no credential. Off when the credential cannot be cleared later. Default on. */
  allowNone?: boolean;
}

/**
 * Chooses the workspace credential a source or destination uses: the credentials that hold every
 * field the type requires, and "New credential" to create one without leaving the form. Nothing
 * secret is ever shown or kept here.
 */
export function CredentialPicker({
  workspaceId,
  fields,
  manifest = false,
  value,
  onChange,
  purpose,
  disabled = false,
  allowNone = true,
}: CredentialPickerProps) {
  const credentials = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/credentials", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const [creating, setCreating] = useState(false);

  function created(credential: Credential) {
    setCreating(false);
    credentials.reload();
    onChange(credential.id);
  }

  return (
    <div className="stack">
      {credentials.status === "loading" ? (
        <LoadingState label="Loading credentials" />
      ) : credentials.status === "error" ? (
        <div className="stack">
          <InlineError title="Could not load the credentials" detail={describeError(credentials.error).detail} />
          <div className="row">
            <Button onClick={credentials.reload}>Try again</Button>
          </div>
        </div>
      ) : (
        <SelectField
          label="Credential"
          hint={
            credentials.data.some((credential) => covers(credential, fields, manifest))
              ? "Shared: rotating it once changes every source and destination that uses it."
              : "No credential holds the fields this type needs yet. Create one."
          }
          value={value}
          onChange={(event) => onChange(event.target.value)}
          disabled={disabled}
        >
          {allowNone || value === "" ? <option value="">Choose a credential</option> : null}
          {credentials.data
            .filter((credential) => credential.id === value || covers(credential, fields, manifest))
            .map((credential) => (
              <option key={credential.id} value={credential.id}>
                {credential.name} ({kindName(null, credential.kind)})
              </option>
            ))}
        </SelectField>
      )}
      <div className="row">
        <Button onClick={() => setCreating(true)} disabled={disabled}>
          New credential
        </Button>
      </div>
      {creating ? (
        <CreateCredentialDialog
          workspaceId={workspaceId}
          schema={schemaFromHints(fields)}
          purpose={purpose}
          onClose={() => setCreating(false)}
          onCreated={created}
        />
      ) : null}
    </div>
  );
}
