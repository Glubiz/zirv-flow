import { useState } from "react";
import { useParams } from "react-router";

import { client, unwrap, useResource } from "../../api";
import { Button, EmptyState, formatDateTime, PageHeader, Panel, Pill, ResourceView } from "../../ui";
import { useWorkspaceAccess } from "../members";
import { ArchiveCredentialDialog } from "./ArchiveCredentialDialog";
import { CreateCredentialDialog } from "./CreateCredentialDialog";
import { EditCredentialDialog } from "./EditCredentialDialog";
import { kindName, maskedHint, testOutcomeLabel, type Credential } from "./labels";
import { RotateCredentialDialog } from "./RotateCredentialDialog";
import { TestCredentialDialog } from "./TestCredentialDialog";

type Action = "edit" | "rotate" | "test" | "archive";

export function CredentialsPage() {
  const { ws = "" } = useParams();
  const access = useWorkspaceAccess(ws);

  return (
    <div className="page">
      <PageHeader
        title="Credentials"
        lede="The keys and secrets of your external accounts. Enter one once, share it between sources and destinations, and rotate it in one place."
      />
      <ResourceView resource={access} loadingLabel="Checking your access">
        {(data) =>
          data.canManage ? (
            <CredentialList key={ws} workspaceId={ws} />
          ) : (
            <Panel flush>
              <EmptyState title="You cannot manage credentials here">
                Only workspace admins and the instance owner can see or change credentials. Ask an admin of this workspace.
              </EmptyState>
            </Panel>
          )
        }
      </ResourceView>
    </div>
  );
}

function CredentialList({ workspaceId }: { workspaceId: string }) {
  const credentials = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/credentials", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const [creating, setCreating] = useState(false);
  const [acting, setActing] = useState<{ action: Action; credential: Credential } | null>(null);

  function done() {
    setActing(null);
    credentials.reload();
  }

  return (
    <>
      <div className="page-actions">
        <Button variant="primary" onClick={() => setCreating(true)}>
          New credential
        </Button>
      </div>
      <Panel flush>
        <ResourceView
          resource={credentials}
          loadingLabel="Loading credentials"
          isEmpty={(data) => data.length === 0}
          empty={
            <EmptyState title="No credentials yet">
              Create a credential for an account a source or destination connects to. Secret values are stored encrypted and
              are never shown again.
            </EmptyState>
          }
        >
          {(data) => (
            <div className="table-scroll">
              <table className="table">
                <caption>Credentials of this workspace</caption>
                <thead>
                  <tr>
                    <th scope="col">Name</th>
                    <th scope="col">Kind</th>
                    <th scope="col">Secret</th>
                    <th scope="col">Used by</th>
                    <th scope="col">Last test</th>
                    <th scope="col">Rotated</th>
                    <th scope="col">
                      <span className="visually-hidden">Actions</span>
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {data.map((credential) => (
                    <CredentialRow key={credential.id} credential={credential} onAct={(action) => setActing({ action, credential })} />
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </ResourceView>
      </Panel>
      {creating ? (
        <CreateCredentialDialog
          workspaceId={workspaceId}
          onClose={() => setCreating(false)}
          onCreated={() => {
            setCreating(false);
            credentials.reload();
          }}
        />
      ) : null}
      {acting?.action === "edit" ? (
        <EditCredentialDialog workspaceId={workspaceId} credential={acting.credential} onClose={() => setActing(null)} onSaved={done} />
      ) : null}
      {acting?.action === "rotate" ? (
        <RotateCredentialDialog workspaceId={workspaceId} credential={acting.credential} onClose={() => setActing(null)} onRotated={done} />
      ) : null}
      {acting?.action === "test" ? (
        <TestCredentialDialog
          workspaceId={workspaceId}
          credential={acting.credential}
          onClose={() => setActing(null)}
          onTested={credentials.reload}
        />
      ) : null}
      {acting?.action === "archive" ? (
        <ArchiveCredentialDialog workspaceId={workspaceId} credential={acting.credential} onClose={() => setActing(null)} onArchived={done} />
      ) : null}
    </>
  );
}

function CredentialRow({ credential, onAct }: { credential: Credential; onAct: (action: Action) => void }) {
  const hints = Object.entries(credential.hints);
  const users = credential.used_by.length;
  return (
    <tr>
      <td className="strong">
        {credential.name}
        {credential.same_secret_as.length > 0 ? (
          <>
            {" "}
            <Pill tone="attention">Same secret as another credential</Pill>
          </>
        ) : null}
      </td>
      <td>{kindName(null, credential.kind)}</td>
      <td className="mono">{hints.length === 0 ? "—" : hints.map(([key, hint]) => `${key} ${maskedHint(hint)}`).join(", ")}</td>
      <td title={credential.used_by.map((user) => user.name).join(", ")}>{users}</td>
      <td>
        {credential.last_tested_at ? (
          <Pill tone={credential.last_test_outcome === "failed" ? "error" : "neutral"}>
            {testOutcomeLabel(credential.last_test_outcome)}
          </Pill>
        ) : (
          testOutcomeLabel(null)
        )}
      </td>
      <td>{formatDateTime(credential.rotated_at)}</td>
      <td className="num">
        <Button onClick={() => onAct("edit")} aria-label={`Edit ${credential.name}`}>
          Edit
        </Button>{" "}
        <Button onClick={() => onAct("rotate")} aria-label={`Rotate ${credential.name}`}>
          Rotate
        </Button>{" "}
        <Button onClick={() => onAct("test")} aria-label={`Test ${credential.name}`}>
          Test
        </Button>{" "}
        <Button onClick={() => onAct("archive")} aria-label={`Archive ${credential.name}`}>
          Archive
        </Button>
      </td>
    </tr>
  );
}
