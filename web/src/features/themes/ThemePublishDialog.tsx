import { useState } from "react";
import { Link } from "react-router";

import { ApiError, client, describeError, unwrap, type BrokenTemplate, type ErrorSummary, type Resource } from "../../api";
import { Button, Dialog, ErrorState, InlineError, LoadingState } from "../../ui";
import { plural } from "../../ui/format";
import { BrokenTemplates, IssueList } from "../templates";
import { nextVersion, reachedTemplates, type Theme, type ThemeIssue, type ThemeUsedBy, type ThemeVersion } from "./model";

/** How many templates the dialog names before the rest go behind "more". */
const SHOWN = 3;

interface Props {
  workspaceId: string;
  theme: Theme;
  /** What uses the theme now. */
  usedBy: Resource<ThemeUsedBy>;
  /** The issues of the saved draft, shown so the person sees what the publish will carry. */
  issues: readonly ThemeIssue[];
  /** Saves any pending edit first; resolves false when it could not. */
  flush: () => Promise<boolean>;
  onClose: () => void;
  onPublished: (theme: Theme, version: ThemeVersion) => void;
}

/** Publishing makes a version that every template using the theme picks up from its next send. */
export function ThemePublishDialog({ workspaceId, theme, usedBy, issues, flush, onClose, onPublished }: Props) {
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [refused, setRefused] = useState<ThemeIssue[]>([]);
  const [more, setMore] = useState(false);
  const [broken, setBroken] = useState<{ detail: string; templates: BrokenTemplate[]; total: number } | null>(null);

  async function publish() {
    setBusy(true);
    setFailure(null);
    setRefused([]);
    setBroken(null);
    try {
      if (!(await flush())) {
        setFailure({ title: "The draft could not be saved", detail: "Fix that, then publish again.", traceId: null });
        setBusy(false);
        return;
      }
      const published = await unwrap(client.POST("/api/v1/workspaces/{ws}/themes/{id}/publish", { params: { path: { ws: workspaceId, id: theme.id } } }));
      onPublished(published.theme, published.version);
    } catch (error) {
      if (error instanceof ApiError && error.code === "theme_invalid") setRefused(error.templateIssues);
      else if (error instanceof ApiError && error.code === "theme_breaks_templates")
        setBroken({ detail: error.detail, templates: error.templates, total: error.total });
      else setFailure(describeError(error));
      setBusy(false);
    }
  }

  const templates = usedBy.status === "ready" ? reachedTemplates(usedBy.data) : [];
  const rest = templates.slice(SHOWN);
  const link = (template: { id: string; name: string }) => (
    <li key={template.id}>
      <Link to={`/w/${workspaceId}/templates/${template.id}`}>{template.name}</Link>
    </li>
  );

  return (
    <Dialog title={`Publish ${theme.name}?`} onClose={onClose}>
      <div className="stack">
        <p>Version {nextVersion(theme)} is used by every email that follows this theme, from its next send.</p>
        {usedBy.status === "loading" ? <LoadingState label="Checking what uses this theme" /> : null}
        {usedBy.status === "error" ? <ErrorState {...describeError(usedBy.error)} onRetry={usedBy.reload} /> : null}
        {usedBy.status === "ready" ? (
          <section className="stack-tight" aria-label="Templates that pick it up">
            <h3 className="field-label">{plural(templates.length, "template picks", "templates pick")} it up</h3>
            <ul className="list-plain" aria-label="First templates">
              {templates.slice(0, SHOWN).map(link)}
            </ul>
            {rest.length > 0 ? (
              <>
                <div>
                  <button type="button" className="link-button" aria-expanded={more} onClick={() => setMore((open) => !open)}>
                    {more ? "Hide the rest" : `${rest.length} more`}
                  </button>
                </div>
                {more ? (
                  <ul className="list-plain" aria-label="More templates">
                    {rest.map(link)}
                  </ul>
                ) : null}
              </>
            ) : null}
          </section>
        ) : null}
        {issues.length > 0 ? <IssueList issues={issues} checked /> : null}
        {refused.length > 0 ? <IssueList issues={refused} checked /> : null}
        {broken ? <BrokenTemplates workspaceId={workspaceId} {...broken} /> : null}
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
