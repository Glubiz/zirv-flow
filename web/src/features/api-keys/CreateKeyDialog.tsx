import { useState, type SyntheticEvent } from "react";

import { client, describeError, unwrap, type components, type ErrorSummary } from "../../api";
import { Button, CopyButton, Dialog, InlineError, TextField } from "../../ui";

type Created = components["schemas"]["CreatedApiKeyBody"];
type Scope = "ingest" | "read" | "write" | "admin";

const SCOPES: { value: Scope; label: string; help: string }[] = [
  { value: "read", label: "Read", help: "Look at data" },
  { value: "write", label: "Write", help: "Change data, except members and keys" },
  { value: "ingest", label: "Ingest", help: "Send data in only" },
  { value: "admin", label: "Admin", help: "Manage members and keys" },
];

interface CreateKeyDialogProps {
  workspaceId: string;
  onClose: () => void;
}

export function CreateKeyDialog({ workspaceId, onClose }: CreateKeyDialogProps) {
  const [name, setName] = useState("");
  const [scopes, setScopes] = useState<Scope[]>(["read"]);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [created, setCreated] = useState<Created | null>(null);

  function toggle(scope: Scope, on: boolean) {
    setScopes((current) => (on ? [...current, scope] : current.filter((item) => item !== scope)));
  }

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      setCreated(
        await unwrap(
          client.POST("/api/v1/workspaces/{ws}/api-keys", {
            params: { path: { ws: workspaceId } },
            body: { name: name.trim(), scopes },
          }),
        ),
      );
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  if (created) {
    return (
      <Dialog title="Copy your new key" onClose={onClose}>
        <p className="muted">
          <span className="strong">You will not see this key again.</span> Copy it now and keep it somewhere safe. If you
          lose it, revoke it and create a new one.
        </p>
        <p className="secret" aria-label={`Secret for ${created.name}`}>
          {created.key}
        </p>
        <div className="dialog-actions">
          <CopyButton value={created.key} label={`Copy the secret for ${created.name}`} />
          <Button variant="primary" onClick={onClose}>
            I have copied it
          </Button>
        </div>
      </Dialog>
    );
  }

  return (
    <Dialog title="Create an API key" onClose={onClose}>
      <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
        <TextField
          label="Name"
          hint="What the key is for, for example the system that will use it."
          value={name}
          onChange={(event) => setName(event.target.value)}
          required
        />
        <fieldset className="checks">
          <legend>Scopes</legend>
          {SCOPES.map((scope) => (
            <label className="check" key={scope.value}>
              <input
                type="checkbox"
                checked={scopes.includes(scope.value)}
                onChange={(event) => toggle(scope.value, event.target.checked)}
              />
              <span>
                {scope.label} <span className="faint">{scope.help}</span>
              </span>
            </label>
          ))}
        </fieldset>
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button type="submit" variant="primary" disabled={busy || scopes.length === 0 || name.trim() === ""}>
            {busy ? "Creating" : "Create key"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
