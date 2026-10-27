import { useState } from "react";
import { useParams } from "react-router";

import { client, describeError, unwrap, useResource, type ErrorSummary } from "../../api";
import { Button, Dialog, EmptyState, InlineError, PageHeader, Panel, Pill, ResourceView } from "../../ui";
import { useWorkspaceAccess } from "../members";
import { precedenceLabel, TraitDialog, typeLabel, type Trait } from "./TraitDialog";

export function TraitsPage() {
  const { ws = "" } = useParams();
  const access = useWorkspaceAccess(ws);
  const canManage = access.status === "ready" && access.data.canAdmin;

  return (
    <div className="page">
      <PageHeader
        title="Fields"
        lede="The fields your sources can fill in on a person. Each has one type and a rule for which source wins when they disagree."
      />
      <TraitList key={ws} workspaceId={ws} canManage={canManage} />
    </div>
  );
}

function TraitList({ workspaceId, canManage }: { workspaceId: string; canManage: boolean }) {
  const traits = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/traits", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const [adding, setAdding] = useState(false);
  const [editing, setEditing] = useState<Trait | null>(null);
  const [archiving, setArchiving] = useState<Trait | null>(null);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [busyKey, setBusyKey] = useState<string | null>(null);

  async function setArchived(trait: Trait, archived: boolean) {
    setBusyKey(trait.key);
    setFailure(null);
    try {
      await unwrap(
        client.PATCH("/api/v1/workspaces/{ws}/traits/{key}", {
          params: { path: { ws: workspaceId, key: trait.key } },
          body: { archived },
        }),
      );
      setArchiving(null);
      traits.reload();
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusyKey(null);
    }
  }

  return (
    <>
      {canManage ? (
        <div className="page-actions">
          <Button variant="primary" onClick={() => setAdding(true)}>
            Add field
          </Button>
        </div>
      ) : null}
      {failure ? <InlineError {...failure} /> : null}
      <Panel flush>
        <ResourceView
          resource={traits}
          loadingLabel="Loading fields"
          isEmpty={(data) => data.length === 0}
          empty={
            <EmptyState title="No fields yet">
              Fields appear here when a source first sends a value for them
              {canManage ? ", or you can add one yourself to choose its type and rules up front." : "."}
            </EmptyState>
          }
        >
          {(data) => (
            <div className="table-scroll">
              <table className="table">
                <caption>Fields of this workspace</caption>
                <thead>
                  <tr>
                    <th scope="col">Key</th>
                    <th scope="col">Type</th>
                    <th scope="col">Winning source</th>
                    <th scope="col">Handling</th>
                    <th scope="col">Status</th>
                    {canManage ? (
                      <th scope="col">
                        <span className="visually-hidden">Actions</span>
                      </th>
                    ) : null}
                  </tr>
                </thead>
                <tbody>
                  {data.map((trait) => (
                    <tr key={trait.key}>
                      <td className="strong mono">
                        {trait.key}
                        {trait.description ? <div className="faint">{trait.description}</div> : null}
                      </td>
                      <td>{typeLabel(trait.type)}</td>
                      <td>
                        {precedenceLabel(trait.precedence)}
                        {trait.precedence === "source_priority" && trait.source_order.length > 0 ? (
                          <div className="faint mono">{trait.source_order.join(" > ")}</div>
                        ) : null}
                      </td>
                      <td>
                        {trait.pii ? <Pill tone="attention">Personal data</Pill> : null}
                        {trait.channel_address ? <Pill>Channel address</Pill> : null}
                        {!trait.pii && !trait.channel_address ? "—" : null}
                      </td>
                      <td>
                        {trait.archived ? <Pill tone="error">Archived</Pill> : <Pill>Active</Pill>}
                        {trait.inferred ? <Pill>Inferred</Pill> : null}
                      </td>
                      {canManage ? (
                        <td className="num">
                          <Button onClick={() => setEditing(trait)} aria-label={`Edit ${trait.key}`}>
                            Edit
                          </Button>{" "}
                          {trait.archived ? (
                            <Button
                              onClick={() => void setArchived(trait, false)}
                              disabled={busyKey === trait.key}
                              aria-label={`Restore ${trait.key}`}
                            >
                              Restore
                            </Button>
                          ) : (
                            <Button onClick={() => setArchiving(trait)} aria-label={`Archive ${trait.key}`}>
                              Archive
                            </Button>
                          )}
                        </td>
                      ) : null}
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </ResourceView>
      </Panel>
      {!canManage ? (
        <p className="muted">Only workspace admins and the instance owner can add or change fields.</p>
      ) : null}
      {adding ? (
        <TraitDialog
          workspaceId={workspaceId}
          onClose={() => setAdding(false)}
          onSaved={() => {
            setAdding(false);
            traits.reload();
          }}
        />
      ) : null}
      {editing ? (
        <TraitDialog
          workspaceId={workspaceId}
          trait={editing}
          onClose={() => setEditing(null)}
          onSaved={() => {
            setEditing(null);
            traits.reload();
          }}
        />
      ) : null}
      {archiving ? (
        <Dialog title={`Archive ${archiving.key}?`} onClose={() => setArchiving(null)}>
          <p className="muted">
            Sources can no longer fill in <span className="mono">{archiving.key}</span>: new values for it are left out. Values
            people already have stay. You can restore the field later.
          </p>
          <div className="dialog-actions">
            <Button onClick={() => setArchiving(null)}>Keep field</Button>
            <Button
              variant="primary"
              onClick={() => void setArchived(archiving, true)}
              disabled={busyKey === archiving.key}
            >
              {busyKey === archiving.key ? "Archiving" : `Archive ${archiving.key}`}
            </Button>
          </div>
        </Dialog>
      ) : null}
    </>
  );
}
