import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Link, useNavigate } from "react-router";

import { ApiError, client, describeError, unwrap, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError } from "../../ui";
import { useLeaveGuard } from "../segments";
import { IssueList, TabBar, blockingCount, isBlocking } from "../templates";
import { asTokens, isArchived, readTokens, statusText, type Theme, type ThemeDetail, type ThemeTokens } from "./model";
import { ArchiveDialog, SetDefaultDialog, ThemeMenu } from "./ThemeActions";
import { ThemePreviewPane, type PreviewState } from "./ThemePreview";
import { ThemePublishDialog } from "./ThemePublishDialog";
import { ThemeSettings } from "./ThemeSettings";
import { WhereUsedView } from "./ThemeUsers";
import { ThemeVersions } from "./ThemeVersions";
import { useThemeUsedBy } from "./useThemes";

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
  initial: ThemeDetail;
  canWrite: boolean;
  canAdmin: boolean;
  /** Reads the theme again, after someone else saved. */
  onReload: () => void;
}

export function ThemeEditor({ workspaceId, initial, canWrite, canAdmin, onReload }: Props) {
  const navigate = useNavigate();
  const [theme, setTheme] = useState<Theme>(initial);
  const [tokens, setTokens] = useState<ThemeTokens>(() => readTokens(initial.draft.tokens));
  const [tab, setTab] = useState<Tab>("editor");
  const [save, setSave] = useState<SaveState>("saved");
  const [saveFailure, setSaveFailure] = useState<ErrorSummary | null>(null);
  const [conflict, setConflict] = useState(false);
  const [preview, setPreview] = useState<PreviewState>({ status: "loading" });
  const [refreshing, setRefreshing] = useState(false);
  const [publishing, setPublishing] = useState(false);
  const [defaulting, setDefaulting] = useState(false);
  const [archiving, setArchiving] = useState(false);
  const [published, setPublished] = useState<string | null>(null);
  const [reloadKey, setReloadKey] = useState(0);
  const usedBy = useThemeUsedBy(workspaceId, theme.id, reloadKey);
  const [settled, setSettled] = useState(false);
  const [jump, setJump] = useState(0);

  const archived = isArchived(theme);
  const editable = canWrite && !archived;
  const page = useRef<HTMLDivElement>(null);
  const tokensRef = useRef(tokens);
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
        client.PUT("/api/v1/workspaces/{ws}/themes/{id}/draft", {
          params: { path: { ws: workspaceId, id: theme.id } },
          body: { tokens: asTokens(tokensRef.current), revision: revision.current },
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
      if (error instanceof ApiError && error.code === "theme_revision_mismatch") {
        stopped.current = true;
        if (live.current) setConflict(true);
      }
      if (live.current) {
        setSave("error");
        setSaveFailure(describeError(error));
      }
      return false;
    }
  }, [workspaceId, theme.id]);

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
      const data = await unwrap(client.POST("/api/v1/workspaces/{ws}/themes/{id}/preview", { params: { path: { ws: workspaceId, id: theme.id } }, body: {} }));
      if (mine === run.current) setPreview({ status: "ready", data });
    } catch (error) {
      if (mine === run.current) setPreview({ status: "failed", error: describeError(error) });
    } finally {
      if (mine === run.current) setRefreshing(false);
    }
  }, [workspaceId, theme.id, editable, canWrite, flush]);

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
      // Leaving inside the debounce, or while a save runs, still saves the newest draft.
      if (dirty.current) void flushNow.current();
    };
  }, []);

  function change(patch: Partial<ThemeTokens>) {
    if (stopped.current) return;
    const next = { ...tokensRef.current, ...patch };
    tokensRef.current = next;
    dirty.current = true;
    saved.current += 1;
    setTokens(next);
    setSave("pending");
    schedule(DEBOUNCE_MS);
  }

  function saveNow() {
    window.clearTimeout(timer.current);
    void refresh();
  }

  const guard = useLeaveGuard(editable && save !== "saved" && !settled);

  useEffect(() => {
    if (settled) void navigate(`/w/${workspaceId}/themes`);
  }, [settled, navigate, workspaceId]);
  const issues = useMemo(() => (preview.status === "ready" ? preview.data.issues : []), [preview]);
  const blocking = blockingCount(issues);
  const warnings = issues.length - blocking;
  const canPublish = editable && !conflict && preview.status === "ready" && blocking === 0 && save === "saved" && !refreshing;
  const publishWhy = canPublish
    ? null
    : blocking > 0
      ? `${blocking} blocking ${blocking === 1 ? "issue" : "issues"} must be fixed first.`
      : save !== "saved"
        ? "Waiting for the draft to be saved."
        : "Waiting for the preview.";

  const wantsWarning = useRef(false);

  function goToWarning() {
    wantsWarning.current = true;
    setTab("editor");
    setJump((n) => n + 1);
  }

  useEffect(() => {
    if (!wantsWarning.current || tab !== "editor") return;
    wantsWarning.current = false;
    const first = issues.findIndex((issue) => !isBlocking(issue));
    const target =
      page.current?.querySelector<HTMLElement>("[data-th-warning]") ?? page.current?.querySelectorAll<HTMLElement>("[data-th-issues] li")[first];
    if (!target) return;
    target.tabIndex = -1;
    target.scrollIntoView({ block: "center" });
    target.focus();
  }, [jump, tab, issues]);

  return (
    <div className="tp-page" ref={page}>
      <div className="tp-head">
        <Link to={`/w/${workspaceId}/themes`}>← Themes</Link>
        <h1 className="fl-name-read">{theme.name}</h1>
        {theme.is_default ? <span className="pill">Default</span> : null}
        <span className="pill">{statusText(theme)}</span>
        <TabBar label="Theme views" variant="page" tabs={TABS} tab={tab} onTab={setTab} />
        <div className="fl-status" role="status">
          {!editable ? (
            <span className="muted">{archived ? "This theme is archived. You can look at it but not change it." : "You can look at this theme but not change it."}</span>
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
        {warnings > 0 ? (
          <button type="button" className="pill pill-attention th-warn-pill" onClick={goToWarning}>
            {warnings} {warnings === 1 ? "warning" : "warnings"}
          </button>
        ) : null}
        {archived ? null : <ThemeMenu theme={theme} canAdmin={canAdmin} onSetDefault={() => setDefaulting(true)} onArchive={() => setArchiving(true)} />}
        {editable ? (
          <Button onClick={saveNow} disabled={conflict || save === "saved" || save === "saving"}>
            Save draft
          </Button>
        ) : null}
        {editable && canAdmin ? (
          <Button variant="primary" disabled={!canPublish} onClick={() => setPublishing(true)} aria-describedby="th-publish-why">
            Publish…
          </Button>
        ) : null}
      </div>
      {publishWhy && editable && canAdmin ? (
        <p id="th-publish-why" className="faint tp-why">
          {publishWhy}
        </p>
      ) : null}
      {conflict ? (
        <div className="tp-banner" role="alert">
          <p>Someone else saved this theme while you were editing. Reload to see their version; your unsaved changes here are lost.</p>
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
        <div className="th-split">
          <ThemeSettings workspaceId={workspaceId} tokens={tokens} issues={issues} readOnly={!editable} onChange={change} />
          <div className="stack">
            {canWrite ? (
              <>
                <ThemePreviewPane state={preview} refreshing={refreshing} onRetry={saveNow} />
                <div data-th-issues tabIndex={-1}>
                  <IssueList issues={issues} checked={preview.status === "ready"} />
                </div>
              </>
            ) : (
              <p className="muted">Members of the workspace can preview a theme.</p>
            )}
          </div>
        </div>
      ) : null}
      {tab === "versions" ? <ThemeVersions workspaceId={workspaceId} themeId={theme.id} reloadKey={reloadKey} /> : null}
      {tab === "used" ? <WhereUsedView workspaceId={workspaceId} themeId={theme.id} reloadKey={reloadKey} /> : null}

      {publishing ? (
        <ThemePublishDialog
          workspaceId={workspaceId}
          theme={theme}
          usedBy={usedBy}
          issues={issues}
          flush={flush}
          onClose={() => setPublishing(false)}
          onPublished={(next, version) => {
            setTheme(next);
            setPublishing(false);
            setReloadKey((key) => key + 1);
            setPublished(`Published version ${version.version}. Emails that follow this theme pick it up from their next send.`);
          }}
        />
      ) : null}
      {defaulting ? (
        <SetDefaultDialog
          workspaceId={workspaceId}
          theme={theme}
          onClose={() => setDefaulting(false)}
          onDone={(next) => {
            setTheme(next);
            setDefaulting(false);
            setReloadKey((key) => key + 1);
          }}
        />
      ) : null}
      {archiving ? (
        <ArchiveDialog
          workspaceId={workspaceId}
          theme={theme}
          onClose={() => setArchiving(false)}
          onArchived={() => {
            window.clearTimeout(timer.current);
            stopped.current = true;
            dirty.current = false;
            setSettled(true);
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
