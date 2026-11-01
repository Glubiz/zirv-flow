import { useState, type SyntheticEvent } from "react";
import { Link, useNavigate, useParams } from "react-router";

import { ApiError, client, describeError, unwrap, type ErrorSummary, type NamedRef } from "../../api";
import { Button, Dialog, EmptyState, InlineError, LibraryNav, PageHeader, Panel, ResourceView, TextField } from "../../ui";
import { plural } from "../../ui/format";
import { useSnippetAccess } from "./access";
import type { Snippet } from "./model";
import { SnippetUserList } from "./SnippetUsers";
import { useSnippets } from "./useSnippets";

export function SnippetsPage() {
  const { ws = "" } = useParams();
  return <SnippetList key={ws} workspaceId={ws} />;
}

function SnippetList({ workspaceId }: { workspaceId: string }) {
  const access = useSnippetAccess(workspaceId);
  const [showArchived, setShowArchived] = useState(false);
  const [adding, setAdding] = useState(false);
  const [archiving, setArchiving] = useState<Snippet | null>(null);
  const snippets = useSnippets(workspaceId, showArchived);

  return (
    <div className="page">
      <LibraryNav workspaceId={workspaceId} />
      <PageHeader
        title="Snippets" breadcrumb={{ group: "Messaging", section: "Snippets" }}
        lede="Blocks you write once and use in many emails. Publish a snippet and every email that holds it uses the new version from its next send."
        actions={
          access.canWrite ? (
            <Button variant="primary" onClick={() => setAdding(true)}>
              New snippet
            </Button>
          ) : undefined
        }
      />
      <div className="page-actions">
        <label className="check">
          <input type="checkbox" checked={showArchived} onChange={(event) => setShowArchived(event.target.checked)} />
          <span>Show archived snippets</span>
        </label>
      </div>
      <Panel flush>
        <ResourceView
          resource={snippets}
          loadingLabel="Loading snippets"
          isEmpty={(data) => data.length === 0}
          empty={
            <EmptyState
              title="No snippets yet"
              action={
                access.canWrite ? (
                  <Button variant="primary" onClick={() => setAdding(true)}>
                    New snippet
                  </Button>
                ) : undefined
              }
            >
              {access.canWrite ? "Make a snippet to use it in many emails." : "Members of the workspace make the snippets."}
            </EmptyState>
          }
        >
          {(data) => (
            <div className="table-scroll">
              <table className="table">
                <caption>Snippets of this workspace</caption>
                <thead>
                  <tr>
                    <th scope="col">Snippet</th>
                    <th scope="col">Version</th>
                    <th scope="col">Used by</th>
                    <th scope="col">
                      <span className="visually-hidden">Actions</span>
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {data.map((snippet) => {
                    const gone = snippet.archived_at !== null && snippet.archived_at !== undefined;
                    return (
                      <tr key={snippet.id}>
                        <td className="strong">
                          <Link to={`/w/${workspaceId}/snippets/${snippet.id}`}>{snippet.name}</Link>
                          {gone ? <span className="faint"> (archived)</span> : null}
                        </td>
                        <td>{snippet.current_version ? `v${snippet.current_version}` : "Draft"}</td>
                        <td>{plural(snippet.used_by_templates, "template", "templates")}</td>
                        <td className="num">
                          {access.canAdmin && !gone ? (
                            <Button aria-label={`Archive ${snippet.name}`} onClick={() => setArchiving(snippet)}>
                              Archive
                            </Button>
                          ) : null}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
        </ResourceView>
      </Panel>
      {adding ? <NewSnippetDialog workspaceId={workspaceId} onClose={() => setAdding(false)} /> : null}
      {archiving ? (
        <ArchiveDialog
          workspaceId={workspaceId}
          snippet={archiving}
          onClose={() => setArchiving(null)}
          onArchived={() => {
            setArchiving(null);
            snippets.reload();
          }}
        />
      ) : null}
    </div>
  );
}

function NewSnippetDialog({ workspaceId, onClose }: { workspaceId: string; onClose: () => void }) {
  const navigate = useNavigate();
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [taken, setTaken] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    if (busy || name.trim() === "") return;
    setBusy(true);
    setFailure(null);
    setTaken(false);
    try {
      const created = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/snippets", { params: { path: { ws: workspaceId } }, body: { name: name.trim() } }),
      );
      void navigate(`/w/${workspaceId}/snippets/${created.id}`);
    } catch (error) {
      if (error instanceof ApiError && error.status === 409) setTaken(true);
      else setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title="New snippet" onClose={onClose}>
      <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
        <TextField
          label="Name"
          hint="Up to 100 characters. Unique among the live snippets."
          value={name}
          onChange={(event) => setName(event.target.value)}
          maxLength={100}
          autoComplete="off"
          required
          error={taken ? "A snippet with that name already exists. Choose another." : undefined}
        />
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button type="submit" variant="primary" disabled={busy || name.trim() === ""}>
            {busy ? "Creating" : "Create snippet"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}

function ArchiveDialog({ workspaceId, snippet, onClose, onArchived }: { workspaceId: string; snippet: Snippet; onClose: () => void; onArchived: () => void }) {
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [holders, setHolders] = useState<{ templates: NamedRef[]; themes: NamedRef[] } | null>(null);

  async function archive() {
    setBusy(true);
    setFailure(null);
    setHolders(null);
    try {
      await unwrap(client.DELETE("/api/v1/workspaces/{ws}/snippets/{id}", { params: { path: { ws: workspaceId, id: snippet.id } } }));
      onArchived();
    } catch (error) {
      if (error instanceof ApiError && error.code === "snippet_in_use") setHolders({ templates: error.snippetTemplates, themes: error.snippetThemes });
      else setFailure(describeError(error));
      setBusy(false);
    }
  }

  const base = `/w/${workspaceId}`;
  const users = holders
    ? [
        ...holders.templates.map((entry) => ({ kind: "template" as const, id: entry.id, name: entry.name, href: `${base}/templates/${entry.id}` })),
        ...holders.themes.map((entry) => ({ kind: "theme" as const, id: entry.id, name: entry.name, href: `${base}/themes/${entry.id}` })),
      ]
    : [];

  return (
    <Dialog title={`Archive ${snippet.name}?`} onClose={onClose}>
      <div className="stack">
        <p className="muted">An archived snippet is kept, with its versions, but cannot be edited or added to a template.</p>
        {holders ? (
          <>
            <p role="alert">It is still in use. Remove it from these first.</p>
            <SnippetUserList users={users} label="Still holding this snippet" />
          </>
        ) : null}
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={() => void archive()} disabled={busy}>
            {busy ? "Archiving" : "Archive"}
          </Button>
        </div>
      </div>
    </Dialog>
  );
}
