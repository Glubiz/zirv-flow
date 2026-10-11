import { useState } from "react";
import { Link, useParams } from "react-router";

import { client, describeError, unwrap, unwrapEmpty, useResource, type ErrorSummary } from "../../api";
import { Button, Dialog, EmptyState, formatDate, InlineError, PageHeader, Panel, ResourceView } from "../../ui";
import { plural, useListAccess } from "./access";
import { ListDialog, type List } from "./ListDialog";

export function ListsPage() {
  const { ws = "" } = useParams();
  return <Lists key={ws} workspaceId={ws} />;
}

function Lists({ workspaceId }: { workspaceId: string }) {
  const { canWrite } = useListAccess(workspaceId);
  const lists = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/lists", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const [creating, setCreating] = useState(false);
  const [renaming, setRenaming] = useState<List | null>(null);
  const [deleting, setDeleting] = useState<List | null>(null);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  async function remove(list: List) {
    setBusy(true);
    setFailure(null);
    try {
      await unwrapEmpty(
        client.DELETE("/api/v1/workspaces/{ws}/lists/{id}", { params: { path: { ws: workspaceId, id: list.id } } }),
      );
      setDeleting(null);
      setNotice(`Deleted ${list.name}.`);
      lists.reload();
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="page">
      <PageHeader
        title="Lists"
        lede="Named sets of people you build by hand, from a CSV or through the API. When two people are merged, the list keeps both of their memberships."
        actions={
          canWrite ? (
            <Button variant="primary" onClick={() => setCreating(true)}>
              New list
            </Button>
          ) : null
        }
      />
      {notice ? (
        <p className="notice" role="status">
          {notice}
        </p>
      ) : null}
      {failure && !deleting ? <InlineError {...failure} /> : null}
      <Panel flush>
        <ResourceView
          resource={lists}
          loadingLabel="Loading lists"
          isEmpty={(data) => data.length === 0}
          empty={
            <EmptyState
              title="No lists yet"
              action={
                canWrite ? (
                  <Button variant="primary" onClick={() => setCreating(true)}>
                    New list
                  </Button>
                ) : null
              }
            >
              A list is a named set of people, such as a newsletter audience. Create one, then add people to it.
            </EmptyState>
          }
        >
          {(data) => (
            <div className="table-scroll">
              <table className="table">
                <caption>Lists of this workspace</caption>
                <thead>
                  <tr>
                    <th scope="col">Name</th>
                    <th scope="col" className="num">
                      People
                    </th>
                    <th scope="col">Created</th>
                    {canWrite ? (
                      <th scope="col">
                        <span className="visually-hidden">Actions</span>
                      </th>
                    ) : null}
                  </tr>
                </thead>
                <tbody>
                  {data.map((list) => (
                    <tr key={list.id}>
                      <td className="strong">
                        <Link to={`/w/${workspaceId}/lists/${list.id}`}>{list.name}</Link>
                        {list.description ? <div className="faint">{list.description}</div> : null}
                      </td>
                      <td className="num">{list.member_count}</td>
                      <td>{formatDate(list.created_at)}</td>
                      {canWrite ? (
                        <td className="num">
                          <Button onClick={() => setRenaming(list)} aria-label={`Edit ${list.name}`}>
                            Edit
                          </Button>{" "}
                          <Button
                            onClick={() => {
                              setFailure(null);
                              setDeleting(list);
                            }}
                            aria-label={`Delete ${list.name}`}
                          >
                            Delete
                          </Button>
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
      {!canWrite && lists.status === "ready" ? (
        <p className="muted">You can look at lists but not change them.</p>
      ) : null}
      {creating ? (
        <ListDialog
          workspaceId={workspaceId}
          onClose={() => setCreating(false)}
          onSaved={(list) => {
            setCreating(false);
            setNotice(`Created ${list.name}.`);
            lists.reload();
          }}
        />
      ) : null}
      {renaming ? (
        <ListDialog
          workspaceId={workspaceId}
          list={renaming}
          onClose={() => setRenaming(null)}
          onSaved={(list) => {
            setRenaming(null);
            setNotice(`Saved ${list.name}.`);
            lists.reload();
          }}
        />
      ) : null}
      {deleting ? (
        <Dialog title={`Delete ${deleting.name}?`} onClose={() => setDeleting(null)}>
          <p className="muted">
            <span className="strong">{deleting.name}</span> has {plural(deleting.member_count, "person", "people")}. Deleting
            it removes the list, who is in it and the history of who joined and left. The people themselves stay. This
            cannot be undone.
          </p>
          {failure ? <InlineError {...failure} /> : null}
          <div className="dialog-actions">
            <Button onClick={() => setDeleting(null)}>Keep list</Button>
            <Button variant="primary" onClick={() => void remove(deleting)} disabled={busy}>
              {busy ? "Deleting" : `Delete ${deleting.name}`}
            </Button>
          </div>
        </Dialog>
      ) : null}
    </div>
  );
}
