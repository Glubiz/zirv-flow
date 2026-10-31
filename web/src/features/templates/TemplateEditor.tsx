import { useCallback, useEffect, useRef, useState } from "react";
import { Link, useNavigate, useSearchParams } from "react-router";

import { ApiError, client, describeError, unwrap, type ErrorSummary } from "../../api";
import { Button, Crumbs, Dialog, InlineError, Pill } from "../../ui";
import { useLeaveGuard } from "../segments";
import { ConvertDialog } from "./ConvertDialog";
import { PreviewPane, type PersonChoice, type PreviewState } from "./PreviewPane";
import { SourcePane } from "./SourcePane";
import { TabBar } from "./TabBar";
import { assistable, TemplateAssistant } from "./TemplateAssistant";
import { SenderOptionsProvider } from "./senderOptions";
import { WhereUsedView } from "./WhereUsed";
import {
  asBody,
  blockingCount,
  modeLabel,
  readDocument,
  statusOf,
  writeDocument,
  type Editable,
  type Template,
  type TemplateDetail,
  type TemplateIssue,
  withSubject,
} from "./model";

/** How long an edit rests before the draft is saved and the preview is asked for again. */
export const DEBOUNCE_MS = 400;

type SaveState = "saved" | "pending" | "saving" | "error";
type Tab = "editor" | "used";

const TABS: readonly { id: Tab; label: string }[] = [
  { id: "editor", label: "Editor" },
  { id: "used", label: "Where used" },
];

interface Props {
  workspaceId: string;
  initial: TemplateDetail;
  canWrite: boolean;
  canAdmin: boolean;
  canPublish: boolean;
  canRealPerson: boolean;
}

const isDraftConflict = (error: unknown) => error instanceof ApiError && error.code === "template_draft_revision_mismatch";

export function TemplateEditor({ workspaceId, initial, canWrite, canAdmin, canPublish: mayPublish, canRealPerson }: Props) {
  const [template, setTemplate] = useState<Template>(initial);
  const [doc, setDoc] = useState<Editable>(() => readDocument(initial.draft.document));
  const [query, setQuery] = useSearchParams();
  const tab: Tab = query.get("tab") === "used" ? "used" : "editor";
  const setTab = (next: Tab) =>
    setQuery(
      (current) => {
        const params = new URLSearchParams(current);
        if (next === "editor") params.delete("tab");
        else params.set("tab", next);
        return params;
      },
      { replace: true },
    );
  const [save, setSave] = useState<SaveState>("saved");
  const [saveFailure, setSaveFailure] = useState<ErrorSummary | null>(null);
  const [preview, setPreview] = useState<PreviewState>({ status: "loading" });
  const [refreshing, setRefreshing] = useState(false);
  const [person, setPerson] = useState<PersonChoice | null>(null);
  const [leaveFor, setLeaveFor] = useState<"test" | "publish" | "history" | null>(null);
  const [goTo, setGoTo] = useState<{ id: string; n: number } | undefined>(undefined);
  const navigate = useNavigate();
  const [converting, setConverting] = useState(false);
  const [published, setPublished] = useState<string | null>(null);
  const [assistantOpen, setAssistantOpen] = useState(false);
  /** Once opened, the panel stays mounted while hidden, so a draft under review survives a look at the editor. */
  const [assistantUsed, setAssistantUsed] = useState(false);
  /** True from the moment an assistant draft is accepted until it lands: nothing is saved or typed meanwhile. */
  const [holding, setHolding] = useState(false);
  const holdingRef = useRef(false);
  /** The revision of the server's draft as the editor last saw it, to tell a lost accept from none. */
  const draftRevision = useRef(initial.draft.revision);
  /** Set when a save found the draft changed by someone else: nothing more is sent until the newer draft is loaded. */
  const [conflict, setConflict] = useState(false);
  const conflictRef = useRef(false);

  const archived = template.archived_at !== null && template.archived_at !== undefined;
  const editable = canWrite && !archived;
  /** Only an admin may draft with the assistant; when it is off the panel says so. */
  const assistantEligible = editable && canAdmin && template.channel === "email" && assistable(doc);
  const docRef = useRef(doc);
  const personRef = useRef<string | null>(null);
  const dirty = useRef(false);
  /** Set once the server converted the draft: no blocks document is saved after that. */
  const convertedRef = useRef(false);
  const revision = useRef(0);
  const run = useRef(0);
  const timer = useRef<number | undefined>(undefined);
  const live = useRef(true);

  /** The saves queued so far: one runs at a time, so an older draft never lands after a newer one. */
  const queue = useRef<Promise<boolean>>(Promise.resolve(true));

  const saveDraft = useCallback(async (): Promise<boolean> => {
    if (!dirty.current) return true;
    if (convertedRef.current && docRef.current.kind === "blocks") return true;
    if (holdingRef.current) return true;
    if (conflictRef.current) return false;
    const sent = revision.current;
    setSave("saving");
    try {
      const saved = await unwrap(
        client.PUT("/api/v1/workspaces/{ws}/templates/{id}/draft", {
          params: { path: { ws: workspaceId, id: template.id } },
          body: { document: asBody(writeDocument(docRef.current)), revision: draftRevision.current },
        }),
      );
      draftRevision.current = saved.draft.revision;
      if (revision.current === sent) {
        dirty.current = false;
        if (live.current) setSave("saved");
      } else if (live.current) {
        setSave("pending");
      }
      if (live.current) setSaveFailure(null);
      return true;
    } catch (error) {
      if (isDraftConflict(error)) markConflict();
      else if (live.current) {
        setSave("error");
        setSaveFailure(describeError(error));
      }
      return false;
    }
  }, [workspaceId, template.id]);

  /** Another change was saved first: the edit stays in the editor and is not sent again. */
  function markConflict() {
    conflictRef.current = true;
    window.clearTimeout(timer.current);
    if (live.current) {
      setConflict(true);
      setSave("error");
      setSaveFailure(null);
    }
  }

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
        client.POST("/api/v1/workspaces/{ws}/templates/{id}/preview", {
          params: { path: { ws: workspaceId, id: template.id } },
          body: personRef.current ? { person_id: personRef.current } : {},
        }),
      );
      if (mine === run.current) setPreview({ status: "ready", data });
    } catch (error) {
      if (mine === run.current) setPreview({ status: "failed", error: describeError(error) });
    } finally {
      if (mine === run.current) setRefreshing(false);
    }
  }, [workspaceId, template.id, editable, canWrite, flush]);

  const schedule = useCallback(
    (ms: number) => {
      window.clearTimeout(timer.current);
      timer.current = window.setTimeout(() => void refresh(), ms);
    },
    [refresh],
  );

  useEffect(() => {
    personRef.current = person?.id ?? null;
    schedule(0);
    return () => window.clearTimeout(timer.current);
    // The first render and every change of the person ask for a preview at once.
  }, [person, schedule]);

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
    if (holdingRef.current) return;
    if (convertedRef.current && next.kind === "blocks") return;
    docRef.current = next;
    dirty.current = true;
    revision.current += 1;
    setDoc(next);
    setSave("pending");
    schedule(DEBOUNCE_MS);
  }

  /** The draft the server converted replaces the blocks the editor held; the editor continues in code mode. */
  function converted(detail: TemplateDetail) {
    const next = readDocument(detail.draft.document);
    convertedRef.current = true;
    draftRevision.current = detail.draft.revision;
    clearTimeout(timer.current);
    docRef.current = next;
    dirty.current = false;
    setDoc(next);
    setTemplate(detail);
    setSave("saved");
    setSaveFailure(null);
    setConverting(false);
    setPublished("Converted to MJML code. The draft is now written as code; published versions are unchanged.");
    schedule(0);
  }

  function setHold(on: boolean) {
    holdingRef.current = on;
    setHolding(on);
  }

  /** Stops the editor changing the draft while an accept runs: no edit is taken and any save that already runs finishes first. */
  async function hold() {
    window.clearTimeout(timer.current);
    setHold(true);
    await queue.current;
  }

  function release() {
    setHold(false);
    if (dirty.current) schedule(DEBOUNCE_MS);
  }

  /** The server's draft replaces the editor's copy; a subject line the person picked from the variants goes on top as an edit of theirs. */
  function adopt(detail: TemplateDetail, subject: string | null) {
    const next = readDocument(detail.draft.document);
    window.clearTimeout(timer.current);
    draftRevision.current = detail.draft.revision;
    conflictRef.current = false;
    setConflict(false);
    docRef.current = next;
    dirty.current = false;
    revision.current += 1;
    setDoc(next);
    setTemplate(detail);
    setSave("saved");
    setSaveFailure(null);
    setHold(false);
    if (subject !== null) change(withSubject(next, subject));
    else schedule(0);
  }

  const readTemplate = () => unwrap(client.GET("/api/v1/workspaces/{ws}/templates/{id}", { params: { path: { ws: workspaceId, id: template.id } } }));

  async function reload() {
    adopt(await readTemplate(), null);
  }

  /** Replaces the edit with the newer draft; a failed read keeps the edit and says so. */
  async function loadNewer() {
    try {
      await reload();
    } catch (error) {
      setSaveFailure(describeError(error));
    }
  }

  /** After a lost reply: the draft the server holds is adopted when it is not the one the editor knows. */
  async function reconcile(subject: string | null): Promise<boolean> {
    try {
      const detail = await readTemplate();
      if (detail.draft.revision === draftRevision.current) return false;
      adopt(detail, subject);
      return true;
    } catch {
      return false;
    }
  }

  /** A save that finds the draft changed ends the conversion before it starts: the notice above says why. */
  async function flushForConvert(): Promise<boolean> {
    const saved = await flush();
    if (!saved && conflictRef.current) setConverting(false);
    return saved;
  }

  function saveNow() {
    window.clearTimeout(timer.current);
    void refresh();
  }

  /** Saves the draft first, so the page it opens works on what the editor shows. */
  async function openPage(page: "test" | "publish" | "history") {
    if (await flush()) setLeaveFor(page);
  }

  const guard = useLeaveGuard(editable && save !== "saved");
  useEffect(() => {
    if (leaveFor !== null && save === "saved") void navigate(`/w/${workspaceId}/templates/${template.id}/${leaveFor}`);
  }, [leaveFor, save, navigate, workspaceId, template.id]);

  const issues: readonly TemplateIssue[] = preview.status === "ready" ? preview.data.issues : [];
  const blocking = blockingCount(issues);
  const status = statusOf(template);
  const canPublish = editable && preview.status === "ready" && blocking === 0 && save === "saved" && !refreshing;
  const publishWhy = !canPublish
    ? blocking > 0
      ? `${blocking} blocking ${blocking === 1 ? "issue" : "issues"} must be fixed first.`
      : save !== "saved"
        ? "Waiting for the draft to be saved."
        : "Waiting for the preview."
    : null;

  return (
    <SenderOptionsProvider workspaceId={workspaceId} enabled={canWrite && template.channel === "email"}>
    <div className="tp-page">
      <div className="tp-head">
        <Crumbs group="Messaging" section="Templates" to={`/w/${workspaceId}/templates`} />
        <h1 className="fl-name-read">{template.name}</h1>
        <span className="fl-state">
          <Pill tone={status.tone}>{status.label}</Pill>
          <span className="faint">{modeLabel(doc)}</span>
        </span>
        <TabBar label="Template views" variant="page" tabs={TABS} tab={tab} onTab={setTab} />
        <Link
          className="fl-tab"
          to={`/w/${workspaceId}/templates/${template.id}/history`}
          onClick={(event) => {
            event.preventDefault();
            void openPage("history");
          }}
        >
          Versions
        </Link>
        <div className="fl-status" role="status">
          {!editable ? (
            <span className="muted">{archived ? "This template is archived. You can look at it but not change it." : "You can look at this template but not change it."}</span>
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
        {assistantEligible ? (
          <Button aria-pressed={assistantOpen} onClick={() => {
              setAssistantOpen((open) => !open);
              setAssistantUsed(true);
            }}>
            Assistant
          </Button>
        ) : null}
        {editable ? (
          <Button onClick={saveNow} disabled={save === "saved" || save === "saving"}>
            Save draft
          </Button>
        ) : null}
        {editable && template.channel === "email" ? (
          <Button onClick={() => void openPage("test")}>Send a test</Button>
        ) : null}
        {editable && mayPublish ? (
          <Button variant="primary" disabled={!canPublish} onClick={() => void openPage("publish")} aria-describedby="tp-publish-why">
            Publish…
          </Button>
        ) : null}
      </div>
      {publishWhy && editable && mayPublish ? (
        <p id="tp-publish-why" className="faint tp-why">
          {publishWhy}
        </p>
      ) : null}
      {conflict ? (
        <div className="notice tp-banner" role="status">
          <span className="row-grow">Another change was saved to this draft. Your edit is still here and has not been saved.</span>
          <Button onClick={() => void loadNewer()}>Load the newer draft</Button>
        </div>
      ) : null}
      {saveFailure ? (
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
          {assistantUsed && assistantEligible ? (
            <div hidden={!assistantOpen}>
            <TemplateAssistant
              workspaceId={workspaceId}
              templateId={template.id}
              doc={doc}
              unsaved={save !== "saved"}
              flush={flush}
              hold={hold}
              release={release}
              onAccepted={adopt}
              onReconcile={reconcile}
              onReload={reload}
              onClose={() => setAssistantOpen(false)}
            />
            </div>
          ) : null}
          {assistantOpen && assistantEligible ? null : (
          <SourcePane workspaceId={workspaceId} doc={doc} onChange={change} readOnly={!editable || holding} onConvert={() => setConverting(true)} issues={issues} checked={preview.status === "ready"} template={template} onTemplate={setTemplate} goTo={goTo} />
          )}
          {canWrite ? (
            <PreviewPane
              templateId={template.id}
              workspaceId={workspaceId}
              state={preview}
              refreshing={refreshing}
              onRetry={saveNow}
              canAdmin={canAdmin}
              canRealPerson={canRealPerson}
              person={person}
              onPerson={setPerson}
              onGoToBlock={doc.kind === "blocks" ? (id) => setGoTo((last) => ({ id, n: (last?.n ?? 0) + 1 })) : undefined}
            />
          ) : (
            <p className="muted">Members of the workspace can preview a template.</p>
          )}
        </div>
      ) : null}
      {tab === "used" ? <WhereUsedView workspaceId={workspaceId} templateId={template.id} reloadKey={0} /> : null}
      {converting ? (
        <ConvertDialog workspaceId={workspaceId} templateId={template.id} revision={() => draftRevision.current} flush={flushForConvert} onConflict={markConflict} onClose={() => setConverting(false)} onConverted={converted} />
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
    </SenderOptionsProvider>
  );
}
