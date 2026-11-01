import { useCallback, useEffect, useRef, useState } from "react";

import { ApiError, client, describeError, unwrap, type ErrorSummary } from "../../api";
import { Button, Crumbs, Dialog, InlineError } from "../../ui";
import { plural } from "../../ui/format";
import { useLeaveGuard } from "../segments";
import { BlockEditor, TabBar, blockingCount, snippetContent, snippetEditable, type Editable, type TemplateIssue } from "../templates";
import { asContent, statusText, type Snippet, type SnippetDetail } from "./model";
import { SnippetPreviewPane, type PreviewState } from "./SnippetPreview";
import { SnippetPublishDialog } from "./SnippetPublishDialog";
import { WhereUsedView } from "./SnippetUsers";
import { SnippetVersions } from "./SnippetVersions";
import { useSnippetUsedBy } from "./useSnippets";

/** How long an edit rests before the draft is saved and the preview is asked for again. */
export const DEBOUNCE_MS = 400;

type SaveState = "saved" | "pending" | "saving" | "error";
type Tab = "editor" | "versions" | "used";

const TABS: readonly { id: Tab; label: string }[] = [
  { id: "editor", label: "Editor" },
  { id: "versions", label: "Versions" },
  { id: "used", label: "Where used" },
];

interface Props {
  workspaceId: string;
  initial: SnippetDetail;
  canWrite: boolean;
  canPublish: boolean;
  /** Reads the snippet again, after someone else saved. */
  onReload: () => void;
}

export function SnippetEditor({ workspaceId, initial, canWrite, canPublish: mayPublish, onReload }: Props) {
  const [snippet, setSnippet] = useState<Snippet>(initial);
  const [doc, setDoc] = useState(() => snippetEditable(initial.draft.content));
  const [tab, setTab] = useState<Tab>("editor");
  const [save, setSave] = useState<SaveState>("saved");
  const [saveFailure, setSaveFailure] = useState<ErrorSummary | null>(null);
  const [conflict, setConflict] = useState(false);
  const [preview, setPreview] = useState<PreviewState>({ status: "loading" });
  const [refreshing, setRefreshing] = useState(false);
  const [publishing, setPublishing] = useState(false);
  const [published, setPublished] = useState<string | null>(null);
  const [reloadKey, setReloadKey] = useState(0);
  const usedBy = useSnippetUsedBy(workspaceId, snippet.id, reloadKey);

  const archived = snippet.archived_at !== null && snippet.archived_at !== undefined;
  const editable = canWrite && !archived;
  const docRef = useRef(doc);
  const dirty = useRef(false);
  const revision = useRef(initial.draft.revision);
  const saved = useRef(0);
  const run = useRef(0);
  const timer = useRef<number | undefined>(undefined);
  const live = useRef(true);
  const stopped = useRef(false);

  /** The saves queued so far: one runs at a time, so an older draft never lands after a newer one. */
  const queue = useRef<Promise<boolean>>(Promise.resolve(true));

  const saveDraft = useCallback(async (): Promise<boolean> => {
    if (!dirty.current) return true;
    if (stopped.current) return false;
    const sent = saved.current;
    setSave("saving");
    try {
      const detail = await unwrap(
        client.PUT("/api/v1/workspaces/{ws}/snippets/{id}/draft", {
          params: { path: { ws: workspaceId, id: snippet.id } },
          body: { content: asContent(snippetContent(docRef.current)), revision: revision.current },
        }),
      );
      revision.current = detail.draft.revision;
      if (saved.current === sent) {
        dirty.current = false;
        if (live.current) setSave("saved");
      } else if (live.current) {
        setSave("pending");
      }
      if (live.current) setSaveFailure(null);
      return true;
    } catch (error) {
      if (error instanceof ApiError && error.code === "snippet_revision_mismatch") {
        stopped.current = true;
        if (live.current) setConflict(true);
      }
      if (live.current) {
        setSave("error");
        setSaveFailure(describeError(error));
      }
      return false;
    }
  }, [workspaceId, snippet.id]);

  /** Saves the draft if it changed, after any save already running. Resolves false when the save failed. */
  const flush = useCallback((): Promise<boolean> => {
    const next = queue.current.then(saveDraft);
    queue.current = next;
    return next;
  }, [saveDraft]);
  const flushNow = useRef(flush);
  useEffect(() => {
    flushNow.current = flush;
  });

  const refresh = useCallback(async () => {
    const mine = ++run.current;
    if (editable && !(await flush())) return;
    if (!canWrite || mine !== run.current || !live.current) return;
    setRefreshing(true);
    try {
      const data = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/snippets/{id}/preview", { params: { path: { ws: workspaceId, id: snippet.id } }, body: {} }),
      );
      if (mine === run.current) setPreview({ status: "ready", data });
    } catch (error) {
      if (mine === run.current) setPreview({ status: "failed", error: describeError(error) });
    } finally {
      if (mine === run.current) setRefreshing(false);
    }
  }, [workspaceId, snippet.id, editable, canWrite, flush]);

  const schedule = useCallback(
    (ms: number) => {
      window.clearTimeout(timer.current);
      timer.current = window.setTimeout(() => void refresh(), ms);
    },
    [refresh],
  );

  useEffect(() => {
    schedule(0);
    return () => window.clearTimeout(timer.current);
  }, [schedule]);

  useEffect(() => {
    live.current = true;
    return () => {
      live.current = false;
      window.clearTimeout(timer.current);
      // Leaving inside the debounce, or while a save runs, still saves the newest content.
      if (dirty.current) void flushNow.current();
    };
  }, []);

  function change(next: Editable) {
    if (next.kind !== "blocks" || stopped.current) return;
    docRef.current = next;
    dirty.current = true;
    saved.current += 1;
    setDoc(next);
    setSave("pending");
    schedule(DEBOUNCE_MS);
  }

  function saveNow() {
    window.clearTimeout(timer.current);
    void refresh();
  }

  const guard = useLeaveGuard(editable && save !== "saved");
  const issues: readonly TemplateIssue[] = preview.status === "ready" ? preview.data.issues : [];
  const blocking = blockingCount(issues);
  const canPublish = editable && !conflict && preview.status === "ready" && blocking === 0 && save === "saved" && !refreshing;
  const publishWhy = canPublish
    ? null
    : blocking > 0
      ? `${blocking} blocking ${blocking === 1 ? "issue" : "issues"} must be fixed first.`
      : save !== "saved"
        ? "Waiting for the draft to be saved."
        : "Waiting for the preview.";

  return (
    <div className="tp-page">
      <div className="tp-head">
        <Crumbs group="Messaging" section="Snippets" to={`/w/${workspaceId}/snippets`} />
        <h1 className="fl-name-read">{snippet.name}</h1>
        <span className="fl-state faint">{statusText(snippet)}</span>
        <TabBar label="Snippet views" variant="page" tabs={TABS} tab={tab} onTab={setTab} />
        <div className="fl-status" role="status">
          {!editable ? (
            <span className="muted">{archived ? "This snippet is archived. You can look at it but not change it." : "You can look at this snippet but not change it."}</span>
          ) : save === "saving" ? (
            <span className="muted">Saving</span>
          ) : save === "pending" ? (
            <span className="muted">Unsaved changes</span>
          ) : save === "error" ? (
            <span className="fl-status-error">Could not save</span>
          ) : (
            <span className="muted">Draft saved</span>
          )}
        </div>
        {editable ? (
          <Button onClick={saveNow} disabled={conflict || save === "saved" || save === "saving"}>
            Save draft
          </Button>
        ) : null}
        {editable && mayPublish ? (
          <Button variant="primary" disabled={!canPublish} onClick={() => setPublishing(true)} aria-describedby="sn-publish-why">
            Publish…
          </Button>
        ) : null}
      </div>
      {publishWhy && editable && mayPublish ? (
        <p id="sn-publish-why" className="faint tp-why">
          {publishWhy}
        </p>
      ) : null}
      {conflict ? (
        <div className="tp-banner" role="alert">
          <p>Someone else saved this snippet while you were editing. Reload to see their version; your unsaved changes here are lost.</p>
          <Button onClick={onReload}>Reload</Button>
        </div>
      ) : saveFailure ? (
        <div className="tp-banner">
          <InlineError {...saveFailure} />
          <Button onClick={saveNow}>Try saving again</Button>
        </div>
      ) : null}
      {published ? (
        <p className="notice" role="status">
          {published}
        </p>
      ) : null}

      {tab === "editor" ? (
        <div className="tp-split">
          <BlockEditor
            mode="snippet"
            workspaceId={workspaceId}
            doc={doc}
            onChange={change}
            readOnly={!editable}
            issues={issues}
            checked={preview.status === "ready"}
            toolbar={
              usedBy.status === "ready" ? (
                <button type="button" className="link-button" onClick={() => setTab("used")}>
                  Used in {plural(usedBy.data.templates.length, "template", "templates")}
                </button>
              ) : null
            }
          />
          {canWrite ? (
            <SnippetPreviewPane state={preview} refreshing={refreshing} onRetry={saveNow} />
          ) : (
            <p className="muted">Members of the workspace can preview a snippet.</p>
          )}
        </div>
      ) : null}
      {tab === "versions" ? <SnippetVersions workspaceId={workspaceId} snippetId={snippet.id} reloadKey={reloadKey} /> : null}
      {tab === "used" ? <WhereUsedView workspaceId={workspaceId} snippetId={snippet.id} reloadKey={reloadKey} /> : null}

      {publishing ? (
        <SnippetPublishDialog
          workspaceId={workspaceId}
          snippet={snippet}
          usedBy={usedBy}
          flush={flush}
          onClose={() => setPublishing(false)}
          onPublished={(next, version) => {
            setSnippet(next);
            setPublishing(false);
            setReloadKey((key) => key + 1);
            setPublished(`Published version ${version.version}. Emails that use this snippet pick it up from their next send.`);
          }}
        />
      ) : null}
      {guard.pending !== null ? (
        <Dialog title="Leave without saving?" onClose={guard.stay}>
          <p className="muted">The draft has changes that are not saved yet. They are lost if you leave now.</p>
          <div className="dialog-actions">
            <Button onClick={guard.stay}>Keep editing</Button>
            <Button variant="primary" onClick={guard.leave}>
              Leave
            </Button>
          </div>
        </Dialog>
      ) : null}
    </div>
  );
}
