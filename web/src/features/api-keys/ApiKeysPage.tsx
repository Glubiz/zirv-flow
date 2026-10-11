import { useState } from "react";
import { useParams } from "react-router";

import { client, describeError, unwrapEmpty, unwrap, useResource, type components, type ErrorSummary } from "../../api";
import {
  Button,
  Dialog,
  EmptyState,
  formatDateTime,
  InlineError,
  PageHeader,
  Panel,
  Pill,
  ResourceView,
  shortId,
} from "../../ui";
import { useWorkspaceAccess } from "../members";
import { CreateKeyDialog } from "./CreateKeyDialog";

type ApiKey = components["schemas"]["ApiKeyBody"];

export function ApiKeysPage() {
  const { ws = "" } = useParams();
  const access = useWorkspaceAccess(ws);

  return (
    <div className="page">
      <PageHeader
        title="API keys"
        lede="Keys let other systems call this workspace. Each key is limited to the scopes you give it."
      />
      <ResourceView resource={access} loadingLabel="Checking your access">
        {(data) =>
          data.canManage ? (
            <KeyList workspaceId={ws} />
          ) : (
            <Panel flush>
              <EmptyState title="You cannot manage API keys here">
                Only workspace admins and the instance owner can see or change keys. Ask an admin of this workspace.
              </EmptyState>
            </Panel>
          )
        }
      </ResourceView>
    </div>
  );
}

function KeyList({ workspaceId }: { workspaceId: string }) {
  const keys = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/api-keys", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const [creating, setCreating] = useState(false);
  const [revoking, setRevoking] = useState<ApiKey | null>(null);

  const createButton = (
    <Button variant="primary" onClick={() => setCreating(true)}>
      Create key
    </Button>
  );

  return (
    <>
      <div className="page-actions">{createButton}</div>
      <Panel flush>
        <ResourceView
          resource={keys}
          loadingLabel="Loading API keys"
          isEmpty={(data) => data.length === 0}
          empty={
            <EmptyState title="No API keys yet">
              Create a key to let a script or another system use this workspace.
            </EmptyState>
          }
        >
          {(data) => (
            <div className="table-scroll">
              <table className="table">
                <caption>API keys of this workspace</caption>
                <thead>
                  <tr>
                    <th scope="col">Name</th>
                    <th scope="col">Key ID</th>
                    <th scope="col">Scopes</th>
                    <th scope="col">Created</th>
                    <th scope="col">Last used</th>
                    <th scope="col">Status</th>
                    <th scope="col">
                      <span className="visually-hidden">Actions</span>
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {data.map((key) => (
                    <tr key={key.id}>
                      <td className="strong">{key.name}</td>
                      <td className="mono">{shortId(key.id)}</td>
                      <td className="mono">{key.scopes.join(", ")}</td>
                      <td>{formatDateTime(key.created_at)}</td>
                      <td>{key.last_used_at ? formatDateTime(key.last_used_at) : "Never"}</td>
                      <td>{key.revoked_at ? <Pill tone="error">Revoked</Pill> : <Pill>Active</Pill>}</td>
                      <td className="num">
                        {key.revoked_at ? null : (
                          <Button onClick={() => setRevoking(key)} aria-label={`Revoke ${key.name}`}>
                            Revoke
                          </Button>
                        )}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </ResourceView>
      </Panel>
      {creating ? (
        <CreateKeyDialog
          workspaceId={workspaceId}
          onClose={() => {
            setCreating(false);
            keys.reload();
          }}
        />
      ) : null}
      {revoking ? (
        <RevokeKeyDialog
          workspaceId={workspaceId}
          apiKey={revoking}
          onClose={() => setRevoking(null)}
          onRevoked={() => {
            setRevoking(null);
            keys.reload();
          }}
        />
      ) : null}
    </>
  );
}

interface RevokeKeyDialogProps {
  workspaceId: string;
  apiKey: ApiKey;
  onClose: () => void;
  onRevoked: () => void;
}

function RevokeKeyDialog({ workspaceId, apiKey, onClose, onRevoked }: RevokeKeyDialogProps) {
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function revoke() {
    setBusy(true);
    setFailure(null);
    try {
      await unwrapEmpty(
        client.DELETE("/api/v1/workspaces/{ws}/api-keys/{id}", {
          params: { path: { ws: workspaceId, id: apiKey.id } },
        }),
      );
      onRevoked();
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title={`Revoke ${apiKey.name}?`} onClose={onClose}>
      <p className="muted">
        Anything using the key <span className="mono">{shortId(apiKey.id)}</span> stops working straight away. This cannot be
        undone.
      </p>
      {failure ? <InlineError {...failure} /> : null}
      <div className="dialog-actions">
        <Button onClick={onClose}>Keep key</Button>
        <Button variant="primary" onClick={() => void revoke()} disabled={busy}>
          {busy ? "Revoking" : `Revoke ${apiKey.name}`}
        </Button>
      </div>
    </Dialog>
  );
}
