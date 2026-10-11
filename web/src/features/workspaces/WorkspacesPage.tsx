import { useState, type SyntheticEvent } from "react";
import { Link } from "react-router";

import { client, describeError, unwrap } from "../../api";
import { Button, Dialog, EmptyState, formatDate, InlineError, PageHeader, Panel, ResourceView, TextField } from "../../ui";
import { useSignedInUser } from "../session";
import { useWorkspaces, type Workspace } from "./WorkspacesProvider";

export function WorkspacesPage() {
  const user = useSignedInUser();
  const workspaces = useWorkspaces();
  const [creating, setCreating] = useState(false);

  const newButton = user.is_owner ? (
    <Button variant="primary" onClick={() => setCreating(true)}>
      New workspace
    </Button>
  ) : null;

  return (
    <div className="page">
      <PageHeader
        title="Workspaces"
        lede="Pick a workspace to see its members and API keys."
        actions={newButton}
      />
      <Panel flush>
        <ResourceView
          resource={workspaces}
          loadingLabel="Loading workspaces"
          isEmpty={(data) => data.length === 0}
          empty={
            <EmptyState title="No workspaces yet">
              {user.is_owner
                ? "Create the first workspace to start adding people and keys."
                : "You are not a member of any workspace. Ask the instance owner to add you."}
            </EmptyState>
          }
        >
          {(data) => (
            <div className="table-scroll">
              <table className="table">
                <caption>Workspaces</caption>
                <thead>
                  <tr>
                    <th scope="col">Name</th>
                    <th scope="col">Slug</th>
                    <th scope="col">Created</th>
                    <th scope="col">
                      <span className="visually-hidden">Open</span>
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {data.map((workspace: Workspace) => (
                    <tr key={workspace.id}>
                      <td className="strong">{workspace.name}</td>
                      <td className="mono">{workspace.slug}</td>
                      <td>{formatDate(workspace.created_at)}</td>
                      <td className="num">
                        <Link to={`/w/${workspace.id}/members`} aria-label={`Open ${workspace.name}`}>
                          Open
                        </Link>
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
        <CreateWorkspaceDialog
          onClose={() => setCreating(false)}
          onCreated={() => {
            setCreating(false);
            workspaces.reload();
          }}
        />
      ) : null}
    </div>
  );
}

function CreateWorkspaceDialog({ onClose, onCreated }: { onClose: () => void; onCreated: () => void }) {
  const [name, setName] = useState("");
  const [slug, setSlug] = useState("");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ReturnType<typeof describeError> | null>(null);

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      await unwrap(
        client.POST("/api/v1/workspaces", {
          body: { name: name.trim(), slug: slug.trim() === "" ? null : slug.trim() },
        }),
      );
      onCreated();
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title="New workspace" onClose={onClose}>
      <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
        <TextField label="Name" value={name} onChange={(event) => setName(event.target.value)} required />
        <TextField
          label="Slug"
          mono
          hint="Optional. Lower case letters, numbers and dashes. Made from the name when left empty."
          value={slug}
          onChange={(event) => setSlug(event.target.value)}
        />
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button type="submit" variant="primary" disabled={busy}>
            {busy ? "Creating" : "Create workspace"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
