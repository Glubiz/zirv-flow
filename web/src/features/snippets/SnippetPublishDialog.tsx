import { useState } from "react";

import { ApiError, client, describeError, unwrap, type ErrorSummary, type ProblemTemplateIssue, type Resource } from "../../api";
import { Button, Dialog, ErrorState, InlineError, LoadingState } from "../../ui";
import { IssueList } from "../templates";
import { nextVersion, usersOf, type Snippet, type SnippetUsedBy, type SnippetVersion } from "./model";
import { SnippetUserList } from "./SnippetUsers";

/** How many users the dialog lists before the rest go behind "more". */
const SHOWN = 3;

interface Props {
  workspaceId: string;
  snippet: Snippet;
  /** What uses the snippet now, read before the dialog opened. */
  usedBy: Resource<SnippetUsedBy>;
  /** Saves any pending edit first; resolves false when it could not. */
  flush: () => Promise<boolean>;
  onClose: () => void;
  onPublished: (snippet: Snippet, version: SnippetVersion) => void;
}

/** Publishing makes a version that every email holding the snippet uses from its next send. */
export function SnippetPublishDialog({ workspaceId, snippet, usedBy, flush, onClose, onPublished }: Props) {
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [issues, setIssues] = useState<ProblemTemplateIssue[]>([]);
  const [more, setMore] = useState(false);

  async function publish() {
    setBusy(true);
    setFailure(null);
    setIssues([]);
    try {
      if (!(await flush())) {
        setFailure({ title: "The draft could not be saved", detail: "Fix that, then publish again.", traceId: null });
        setBusy(false);
        return;
      }
      const published = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/snippets/{id}/publish", { params: { path: { ws: workspaceId, id: snippet.id } } }),
      );
      onPublished(published.snippet, published.version);
    } catch (error) {
      if (error instanceof ApiError && error.code === "snippet_invalid") setIssues(error.templateIssues);
      else setFailure(describeError(error));
      setBusy(false);
    }
  }

  const users = usedBy.status === "ready" ? usersOf(workspaceId, usedBy.data) : [];
  const rest = users.slice(SHOWN);

  return (
    <Dialog title={`Publish ${snippet.name}?`} onClose={onClose}>
      <div className="stack">
        <p>Version {nextVersion(snippet)} goes into every email that uses it, from its next send.</p>
        {usedBy.status === "loading" ? <LoadingState label="Checking what uses this snippet" /> : null}
        {usedBy.status === "error" ? <ErrorState {...describeError(usedBy.error)} onRetry={usedBy.reload} /> : null}
        {usedBy.status === "ready" ? (
          <section className="stack" aria-label="Used in">
            <h3 className="field-label">Used in</h3>
            {users.length === 0 ? <p className="muted">Nothing uses it yet.</p> : null}
            <SnippetUserList users={users.slice(0, SHOWN)} label="First users" />
            {rest.length > 0 ? (
              <>
                <div>
                  <button type="button" className="link-button" aria-expanded={more} onClick={() => setMore((open) => !open)}>
                    {more ? "Hide the rest" : `${rest.length} more`}
                  </button>
                </div>
                {more ? <SnippetUserList users={rest} label="More users" /> : null}
              </>
            ) : null}
          </section>
        ) : null}
        {issues.length > 0 ? <IssueList issues={issues} checked /> : null}
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={() => void publish()} disabled={busy || usedBy.status === "loading"}>
            {busy ? "Publishing" : "Publish"}
          </Button>
        </div>
      </div>
    </Dialog>
  );
}
