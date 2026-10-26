import { useState } from "react";

import { client, describeError, unwrap, useResource } from "../../api";
import { Button, EmptyState, ErrorState, LoadingState, ResourceView } from "../../ui";
import { formatDateTime } from "../../ui/format";
import { BlockEditor, snippetEditable } from "../templates";

interface Props {
  workspaceId: string;
  snippetId: string;
  /** Changes whenever a version is published, to reload the list. */
  reloadKey: number;
}

/** The published versions, and a read-only look at one. */
export function SnippetVersions({ workspaceId, snippetId, reloadKey }: Props) {
  const [open, setOpen] = useState<number | null>(null);
  const versions = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/snippets/{id}/versions", { params: { path: { ws: workspaceId, id: snippetId } } })),
    [workspaceId, snippetId, reloadKey],
  );

  if (open !== null) {
    return <VersionDetail workspaceId={workspaceId} snippetId={snippetId} number={open} onBack={() => setOpen(null)} />;
  }
  return (
    <ResourceView
      resource={versions}
      loadingLabel="Loading versions"
      isEmpty={(data) => data.length === 0}
      empty={<EmptyState title="Nothing published yet">Publish the draft to make version 1.</EmptyState>}
    >
      {(data) => (
        <div className="table-scroll">
          <table className="table">
            <caption>Published versions</caption>
            <thead>
              <tr>
                <th scope="col">Version</th>
                <th scope="col">Published</th>
                <th scope="col">Warnings</th>
                <th scope="col">
                  <span className="visually-hidden">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody>
              {data.map((version) => (
                <tr key={version.version}>
                  <td className="strong">Version {version.version}</td>
                  <td>{formatDateTime(version.published_at)}</td>
                  <td>{version.warnings}</td>
                  <td className="num">
                    <Button aria-label={`View version ${version.version}`} onClick={() => setOpen(version.version)}>
                      View
                    </Button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </ResourceView>
  );
}

function VersionDetail({ workspaceId, snippetId, number, onBack }: { workspaceId: string; snippetId: string; number: number; onBack: () => void }) {
  const version = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/snippets/{id}/versions/{n}", { params: { path: { ws: workspaceId, id: snippetId, n: number } } }),
      ),
    [workspaceId, snippetId, number],
  );
  return (
    <section className="stack" aria-label={`Version ${number}`}>
      <div className="row">
        <Button onClick={onBack}>Back to the versions</Button>
        <h2 className="panel-title">Version {number}, read only</h2>
      </div>
      {version.status === "loading" ? <LoadingState label="Loading the version" /> : null}
      {version.status === "error" ? <ErrorState {...describeError(version.error)} onRetry={version.reload} /> : null}
      {version.status === "ready" ? (
        <>
          <p className="faint">Published {formatDateTime(version.data.published_at)}. A published version never changes.</p>
          <BlockEditor
            mode="snippet"
            workspaceId={workspaceId}
            doc={snippetEditable(version.data.content)}
            onChange={() => undefined}
            readOnly
            issues={version.data.warnings}
            checked
          />
        </>
      ) : null}
    </section>
  );
}
