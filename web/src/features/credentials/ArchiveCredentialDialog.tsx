import { useState } from "react";

import { ApiError, client, describeError, unwrap, unwrapEmpty, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError } from "../../ui";
import type { Credential } from "./labels";

interface ArchiveCredentialDialogProps {
  workspaceId: string;
  credential: Credential;
  onClose: () => void;
  onArchived: () => void;
}

/**
 * Archives a credential. The server refuses while a live source or destination uses it; the
 * dialog then reads the credential again and lists what uses it.
 */
export function ArchiveCredentialDialog({ workspaceId, credential, onClose, onArchived }: ArchiveCredentialDialogProps) {
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [usedBy, setUsedBy] = useState<Credential["used_by"] | null>(null);

  async function archive() {
    setBusy(true);
    setFailure(null);
    try {
      await unwrapEmpty(
        client.DELETE("/api/v1/workspaces/{ws}/credentials/{id}", {
          params: { path: { ws: workspaceId, id: credential.id } },
        }),
      );
      onArchived();
    } catch (error) {
      if (error instanceof ApiError && error.code === "credential_in_use") {
        try {
          const fresh = await unwrap(
            client.GET("/api/v1/workspaces/{ws}/credentials/{id}", {
              params: { path: { ws: workspaceId, id: credential.id } },
            }),
          );
          setUsedBy(fresh.used_by);
        } catch (reading) {
          setFailure(describeError(reading));
        }
      } else {
        setFailure(describeError(error));
      }
      setBusy(false);
    }
  }

  return (
    <Dialog title={`Archive ${credential.name}?`} onClose={onClose}>
      <p className="muted">
        An archived credential cannot be used by a new source or destination. Its name is free again.
      </p>
      {usedBy ? (
        <div className="stack" role="alert">
          <p className="strong">This credential is still used, so it was not archived.</p>
          <p className="muted">Change these to use another credential, or remove them, then try again.</p>
          <ul className="list-plain" aria-label="Used by">
            {usedBy.map((user) => (
              <li key={`${user.kind}:${user.id}`}>
                {user.name} <span className="faint">({user.kind})</span>
              </li>
            ))}
          </ul>
        </div>
      ) : null}
      {failure ? <InlineError {...failure} /> : null}
      <div className="dialog-actions">
        <Button onClick={onClose}>Keep credential</Button>
        <Button variant="primary" onClick={() => void archive()} disabled={busy}>
          {busy ? "Archiving" : `Archive ${credential.name}`}
        </Button>
      </div>
    </Dialog>
  );
}
