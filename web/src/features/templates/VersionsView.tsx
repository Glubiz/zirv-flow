import { useState } from "react";

import { client, describeError, unwrap, useResource } from "../../api";
import { Button, EmptyState, ErrorState, LoadingState, ResourceView } from "../../ui";
import { formatDateTime } from "../../ui/format";
import { issueText, readDocument, type TemplateVersion } from "./model";

interface Props {
  workspaceId: string;
  templateId: string;
  /** Changes whenever a version is published, to reload the list. */
  reloadKey: number;
}

/** The published versions, and a read-only look at one. */
export function VersionsView({ workspaceId, templateId, reloadKey }: Props) {
  const [open, setOpen] = useState<number | null>(null);
  const versions = useResource(
    () =>
      unwrap(client.GET("/api/v1/workspaces/{ws}/templates/{id}/versions", { params: { path: { ws: workspaceId, id: templateId } } })),
    [workspaceId, templateId, reloadKey],
  );

  if (open !== null) {
    return <VersionDetail workspaceId={workspaceId} templateId={templateId} number={open} onBack={() => setOpen(null)} />;
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

function VersionDetail({ workspaceId, templateId, number, onBack }: { workspaceId: string; templateId: string; number: number; onBack: () => void }) {
  const version = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/templates/{id}/versions/{n}", {
          params: { path: { ws: workspaceId, id: templateId, n: number } },
        }),
      ),
    [workspaceId, templateId, number],
  );
  return (
    <section className="stack" aria-label={`Version ${number}`}>
      <div className="row">
        <Button onClick={onBack}>Back to the versions</Button>
        <h2 className="panel-title">Version {number}, read only</h2>
      </div>
      {version.status === "loading" ? <LoadingState label="Loading the version" /> : null}
      {version.status === "error" ? <ErrorState {...describeError(version.error)} onRetry={version.reload} /> : null}
      {version.status === "ready" ? <VersionBody version={version.data} /> : null}
    </section>
  );
}

function VersionBody({ version }: { version: TemplateVersion }) {
  const doc = readDocument(version.document);
  return (
    <div className="stack">
      <p className="faint">Published {formatDateTime(version.published_at)}. A published version never changes.</p>
      {version.warnings.length > 0 ? (
        <ul className="list-plain" aria-label="Warnings when it was published">
          {version.warnings.map((warning, index) => (
            <li key={index} className="field-hint">
              Warning: {issueText(warning)}
            </li>
          ))}
        </ul>
      ) : null}
      {doc.kind === "email" ? (
        <>
          <ReadOnly label="Subject" value={doc.email.subject} />
          <ReadOnly label={doc.mode === "mjml" ? "MJML source" : "HTML source"} value={doc.email.source} />
          <ReadOnly label="Text part" value={doc.email.text} />
        </>
      ) : null}
      {doc.kind === "webhook" ? (
        <>
          <ReadOnly label="Headers" value={doc.webhook.headers.map((header) => `${header.name}: ${header.value}`).join("\n")} />
          <ReadOnly label="JSON body" value={doc.webhook.body} />
        </>
      ) : null}
      {doc.kind === "unsupported" ? <p className="muted">This version is written in a mode this editor cannot show yet.</p> : null}
    </div>
  );
}

function ReadOnly({ label, value }: { label: string; value: string }) {
  return (
    <div className="field">
      <span className="field-label">{label}</span>
      <pre className="fl-code tp-text" aria-label={label}>
        {value}
      </pre>
    </div>
  );
}
