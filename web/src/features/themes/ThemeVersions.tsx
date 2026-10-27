import { useEffect, useState } from "react";

import { client, describeError, unwrap, useResource } from "../../api";
import { Button, EmptyState, ErrorState, LoadingState, ResourceView } from "../../ui";
import { formatDateTime } from "../../ui/format";
import { IssueList } from "../templates";
import { readTokens, type ThemeTokens } from "./model";
import { ThemePreviewPane, type PreviewState } from "./ThemePreview";

interface Props {
  workspaceId: string;
  themeId: string;
  /** Changes whenever a version is published, to reload the list. */
  reloadKey: number;
  /** The theme preview route needs Edit; without it a version shows its tokens only. */
  canPreview: boolean;
}

/** The published versions, and a read-only look at one. */
export function ThemeVersions({ workspaceId, themeId, reloadKey, canPreview }: Props) {
  const [open, setOpen] = useState<number | null>(null);
  const versions = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/themes/{id}/versions", { params: { path: { ws: workspaceId, id: themeId } } })),
    [workspaceId, themeId, reloadKey],
  );

  if (open !== null) {
    return <VersionDetail workspaceId={workspaceId} themeId={themeId} number={open} canPreview={canPreview} onBack={() => setOpen(null)} />;
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

function VersionDetail({ workspaceId, themeId, number, canPreview, onBack }: { workspaceId: string; themeId: string; number: number; canPreview: boolean; onBack: () => void }) {
  const version = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/themes/{id}/versions/{n}", { params: { path: { ws: workspaceId, id: themeId, n: number } } })),
    [workspaceId, themeId, number],
  );
  const [preview, setPreview] = useState<PreviewState>({ status: "loading" });
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    if (!canPreview) return;
    let current = true;
    unwrap(
      client.POST("/api/v1/workspaces/{ws}/themes/{id}/preview", { params: { path: { ws: workspaceId, id: themeId } }, body: { version: number } }),
    ).then(
      (data) => current && setPreview({ status: "ready", data }),
      (error: unknown) => current && setPreview({ status: "failed", error: describeError(error) }),
    );
    return () => {
      current = false;
    };
  }, [workspaceId, themeId, number, attempt, canPreview]);

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
          <div className="th-version">
            <div className="stack">
              <TokenList tokens={readTokens(version.data.tokens)} />
              <IssueList issues={version.data.warnings} checked />
            </div>
            {canPreview ? (
              <ThemePreviewPane
                state={preview}
                refreshing={false}
                onRetry={() => {
                  setPreview({ status: "loading" });
                  setAttempt((n) => n + 1);
                }}
                frameTitle={`Preview of version ${number}`}
              />
            ) : null}
          </div>
        </>
      ) : null}
    </section>
  );
}

const TOKEN_ROWS: readonly { label: string; text: (tokens: ThemeTokens) => string }[] = [
  { label: "Primary", text: (t) => t.primary },
  { label: "Background", text: (t) => t.background },
  { label: "Text", text: (t) => t.text },
  { label: "Link", text: (t) => t.link },
  { label: "Button", text: (t) => t.button },
  { label: "Button text", text: (t) => t.button_text },
  { label: "Font", text: (t) => t.font },
  { label: "Content width", text: (t) => `${t.content_width} px` },
  { label: "Button corners", text: (t) => `${t.button_radius} px` },
  { label: "Underline links", text: (t) => (t.link_underline ? "Yes" : "No") },
  { label: "Logo", text: (t) => (t.logo ? "Set" : "None") },
  { label: "Footer", text: (t) => (t.footer_snippet ? "A snippet" : "None") },
];

/** The tokens of a published version, as words and values. */
function TokenList({ tokens }: { tokens: ThemeTokens }) {
  return (
    <dl className="th-tokens" aria-label="Tokens of this version">
      {TOKEN_ROWS.map((row) => (
        <div key={row.label} className="th-token">
          <dt className="faint">{row.label}</dt>
          <dd>{row.text(tokens)}</dd>
        </div>
      ))}
    </dl>
  );
}
