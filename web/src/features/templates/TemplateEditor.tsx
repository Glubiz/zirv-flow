import { useCallback, useEffect, useRef, useState } from "react";
import { Link } from "react-router";

import { client, describeError, unwrap, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError, Pill } from "../../ui";
import { useLeaveGuard } from "../segments";
import { ConvertDialog } from "./ConvertDialog";
import { PreviewPane, type PersonChoice, type PreviewState } from "./PreviewPane";
import { PublishDialog } from "./PublishDialog";
import { SourcePane } from "./SourcePane";
import { TabBar } from "./TabBar";
import { assistable, TemplateAssistant } from "./TemplateAssistant";
import { SenderOptionsProvider } from "./senderOptions";
import { TestSendDialog } from "./TestSendDialog";
import { VersionsView } from "./VersionsView";
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
type Tab = "editor" | "versions" | "used";

const TABS: readonly { id: Tab; label: string }[] = [
  { id: "editor", label: "Editor" },
  { id: "versions", label: "Versions" },
  { id: "used", label: "Where used" },
];

interface Props {
  workspaceId: string;
  initial: TemplateDetail;
  canWrite: boolean;
  canAdmin: boolean;
}

export function TemplateEditor({ workspaceId, initial, canWrite, canAdmin }: Props) {
  const [template, setTemplate] = useState<Template>(initial);
  const [doc, setDoc] = useState<Editable>(() => readDocument(initial.draft.document));
  const [tab, setTab] = useState<Tab>("editor");
  const [save, setSave] = useState<SaveState>("saved");
  const [saveFailure, setSaveFailure] = useState<ErrorSummary | null>(null);
  const [preview, setPreview] = useState<PreviewState>({ status: "loading" });
  const [refreshing, setRefreshing] = useState(false);
  const [person, setPerson] = useState<PersonChoice | null>(null);
  const [goTo, setGoTo] = useState<{ id: string; n: number } | undefined>(undefined);
  const [publishing, setPublishing] = useState(false);
  const [testing, setTesting] = useState(false);
  const [converting, setConverting] = useState(false);
  const [published, setPublished] = useState<string | null>(null);
  const [versionsKey, setVersionsKey] = useState(0);
  const [assistantOpen, setAssistantOpen] = useState(false);
  /** True from the moment an assistant draft is accepted until it lands: nothing is saved or typed meanwhile. */
  const [holding, setHolding] = useState(false);
  const holdingRef = useRef(false);
  /** The revision of the server's draft as the editor last saw it, to tell a lost accept from none. */
  const draftRevision = useRef(initial.draft.revision);

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
    const sent = revision.current;
    setSave("saving");
    try {
      const saved = await unwrap(
        client.PUT("/api/v1/workspaces/{ws}/templates/{id}/draft", {
          params: { path: { ws: workspaceId, id: template.id } },
          body: { document: asBody(writeDocument(docRef.current)) },
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
      if (live.current) {
        setSave("error");
        setSaveFailure(describeError(error));
      }
      return false;
    }
  }, [workspaceId, template.id]);

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

  function saveNow() {
    window.clearTimeout(timer.current);
    void refresh();
  }

  const guard = useLeaveGuard(editable && save !== "saved");
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
        <Link to={`/w/${workspaceId}/templates`}>← Templates</Link>
        <h1 className="fl-name-read">{template.name}</h1>
        <span className="fl-state">
          <Pill tone={status.tone}>{status.label}</Pill>
          <span className="faint">{modeLabel(doc)}</span>
        </span>
        <TabBar label="Template views" variant="page" tabs={TABS} tab={tab} onTab={setTab} />
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
          <Button aria-pressed={assistantOpen} onClick={() => setAssistantOpen((open) => !open)}>
            Assistant
          </Button>
        ) : null}
        {editable ? (
          <Button onClick={saveNow} disabled={save === "saved" || save === "saving"}>
            Save draft
          </Button>
        ) : null}
        {editable && template.channel === "email" ? (
          <Button onClick={() => setTesting(true)}>Test send</Button>
        ) : null}
        {editable && canAdmin ? (
          <Button variant="primary" disabled={!canPublish} onClick={() => setPublishing(true)} aria-describedby="tp-publish-why">
            Publish…
          </Button>
        ) : null}
      </div>
      {publishWhy && editable && canAdmin ? (
        <p id="tp-publish-why" className="faint tp-why">
          {publishWhy}
        </p>
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
          {assistantOpen && assistantEligible ? (
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
          ) : (
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
              person={person}
              onPerson={setPerson}
              onGoToBlock={doc.kind === "blocks" ? (id) => setGoTo((last) => ({ id, n: (last?.n ?? 0) + 1 })) : undefined}
            />
          ) : (
            <p className="muted">Members of the workspace can preview a template.</p>
          )}
        </div>
      ) : null}
      {tab === "versions" ? <VersionsView workspaceId={workspaceId} templateId={template.id} reloadKey={versionsKey} /> : null}
      {tab === "used" ? <WhereUsedView workspaceId={workspaceId} templateId={template.id} reloadKey={versionsKey} /> : null}

      {publishing ? (
        <PublishDialog
          workspaceId={workspaceId}
          template={template}
          flush={flush}
          onClose={() => setPublishing(false)}
          onPublished={(next, version) => {
            setTemplate(next);
            setPublishing(false);
            setVersionsKey((key) => key + 1);
            setPublished(`Published version ${version.version}. Flows that send this template use it from their next send.`);
          }}
        />
      ) : null}
      {converting ? (
        <ConvertDialog workspaceId={workspaceId} templateId={template.id} flush={flush} onClose={() => setConverting(false)} onConverted={converted} />
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
      {testing ? <TestSendDialog workspaceId={workspaceId} template={template} flush={flush} personId={person?.id ?? null} onClose={() => setTesting(false)} /> : null}
    </div>
    </SenderOptionsProvider>
  );
}
