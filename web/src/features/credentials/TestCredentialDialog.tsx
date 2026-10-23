import { useState, type SyntheticEvent } from "react";

import { client, describeError, unwrap, type components, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError, SelectField } from "../../ui";
import type { Credential } from "./labels";

type Outcome = components["schemas"]["CredentialTestBody"];

interface TestCredentialDialogProps {
  workspaceId: string;
  credential: Credential;
  onClose: () => void;
  /** Called after a test ran, so the list can show the new outcome. */
  onTested: () => void;
}

/** Tests a credential by running the check of one source or destination that uses it. */
export function TestCredentialDialog({ workspaceId, credential, onClose, onTested }: TestCredentialDialogProps) {
  const users = credential.used_by;
  const [chosen, setChosen] = useState(users[0] ? `${users[0].kind}:${users[0].id}` : "");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [outcome, setOutcome] = useState<Outcome | null>(null);

  async function run(event: SyntheticEvent) {
    event.preventDefault();
    const [kind, id] = chosen.split(":");
    if (!id) return;
    setBusy(true);
    setFailure(null);
    setOutcome(null);
    try {
      setOutcome(
        await unwrap(
          client.POST("/api/v1/workspaces/{ws}/credentials/{id}/test", {
            params: { path: { ws: workspaceId, id: credential.id } },
            body: kind === "source" ? { source_id: id } : { destination_id: id },
          }),
        ),
      );
      onTested();
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog title={`Test ${credential.name}`} onClose={onClose}>
      {users.length === 0 ? (
        <>
          <p className="muted">
            Nothing uses this credential yet, so there is nothing to test it with. Choose it on a source or destination first.
          </p>
          <div className="dialog-actions">
            <Button onClick={onClose}>Close</Button>
          </div>
        </>
      ) : (
        <form className="stack" onSubmit={(event) => void run(event)}>
          <SelectField
            label="Test with"
            hint="The check of this source or destination runs with the credential."
            value={chosen}
            onChange={(event) => setChosen(event.target.value)}
            disabled={busy}
          >
            {users.map((user) => (
              <option key={`${user.kind}:${user.id}`} value={`${user.kind}:${user.id}`}>
                {user.name} ({user.kind})
              </option>
            ))}
          </SelectField>
          {outcome ? (
            outcome.ok ? (
              <p className="notice" role="status">
                The check passed.
              </p>
            ) : (
              <InlineError title="The check failed" detail={outcome.message ?? undefined} />
            )
          ) : null}
          {failure ? <InlineError {...failure} /> : null}
          <div className="dialog-actions">
            <Button onClick={onClose}>Close</Button>
            <Button type="submit" variant="primary" disabled={busy || chosen === ""}>
              {busy ? "Testing" : "Run test"}
            </Button>
          </div>
        </form>
      )}
    </Dialog>
  );
}
