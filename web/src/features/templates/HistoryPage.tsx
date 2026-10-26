import { useMemo, useState } from "react";
import { Link, useParams } from "react-router";

import { ApiError, client, describeError, unwrap, useResource, type ErrorSummary } from "../../api";
import { Button, InlineError, LoadingState, ResourceView } from "../../ui";
import { useMembers } from "../members";
import { useTemplateAccess } from "./access";
import { diffDocuments } from "./diff";
import { DetailsPanel, draftRow, HistoryList, UsedByPanel, versionRow } from "./HistoryPanels";
import { asBody, type TemplateDetail, type TemplateVersion, type TemplateVersionSummary } from "./model";
import { labelOf } from "./PreviewPane";
import { Frame } from "./PublishPanels";
import { createLimiter, MAX_IN_FLIGHT, SAMPLE_SUBJECT, type Subject } from "./reviewChecks";
import { usePreviews } from "./usePreviews";
import { snippetIds, themeChoice } from "./versionUse";
import { useWhereUsed } from "./WhereUsed";

type Selection = "draft" | number;

interface Draft {
  document: Record<string, unknown>;
  revision: number;
  savedAt: string;
}

/** What the last copy replaced, kept in memory so Undo can put it back. */
interface Copied {
  version: number;
  before: Draft;
  /** The revision the copy returned, which the undo must name. */
  revision: number;
}

interface Failure {
  summary: ErrorSummary;
  /** Someone else saved the draft since this page read it. */
  conflict: boolean;
}

const authorOf = (authors: ReadonlyMap<string, string>, userId: string | null | undefined): string => (userId ? (authors.get(userId) ?? "A former member") : "An API key");

const fromDetail = (detail: TemplateDetail): Draft => ({ document: detail.draft.document, revision: detail.draft.revision, savedAt: detail.draft.updated_at });

const idPath = (workspaceId: string, templateId: string) => ({ params: { path: { ws: workspaceId, id: templateId } } });

export function HistoryPage() {
  const { ws = "", id = "" } = useParams();
  const access = useTemplateAccess(ws);
  const detail = useResource(() => unwrap(client.GET("/api/v1/workspaces/{ws}/templates/{id}", idPath(ws, id))), [ws, id]);
  const versions = useResource(() => unwrap(client.GET("/api/v1/workspaces/{ws}/templates/{id}/versions", idPath(ws, id))), [ws, id]);
  const members = useMembers(ws);
  const authors = useMemo(() => new Map(members.status === "ready" ? members.data.map((member) => [member.user_id, member.email]) : []), [members]);
  return (
    <ResourceView resource={access.ready ? detail : { status: "loading", reload: detail.reload }} loadingLabel="Loading the history">
      {(template) => (
        <ResourceView resource={versions} loadingLabel="Loading the history">
          {(list) => <History key={`${ws}:${id}`} workspaceId={ws} template={template} versions={list} authors={authors} canWrite={access.canWrite} canAdmin={access.canAdmin} />}
        </ResourceView>
      )}
    </ResourceView>
  );
}

interface Props {
  workspaceId: string;
  template: TemplateDetail;
  versions: readonly TemplateVersionSummary[];
  authors: ReadonlyMap<string, string>;
  canWrite: boolean;
  canAdmin: boolean;
}

/** Reads the live version, which the draft row counts its changes against. */
function History(props: Props) {
  const { workspaceId, template } = props;
  const live = template.current_version ?? null;
  const liveVersion = useResource(
    async (): Promise<TemplateVersion | null> =>
      live === null ? null : unwrap(client.GET("/api/v1/workspaces/{ws}/templates/{id}/versions/{n}", { params: { path: { ws: workspaceId, id: template.id, n: live } } })),
    [workspaceId, template.id, live],
  );
  return (
    <div className="tp-page">
      <div className="tp-head">
        <Link to={`/w/${workspaceId}/templates/${template.id}`}>← Back to editing</Link>
        <div className="row-grow">
          <p className="faint">{template.name}</p>
          <h1 className="fl-name-read">Versions and use</h1>
        </div>
      </div>
      <ResourceView resource={liveVersion} loadingLabel="Loading the versions">
        {(version) => <Body {...props} liveDocument={version?.document ?? null} />}
      </ResourceView>
    </div>
  );
}

function Body({ workspaceId, template, versions, authors, canWrite, canAdmin, liveDocument }: Props & { liveDocument: unknown }) {
  const live = template.current_version ?? null;
  const archived = template.archived_at !== null && template.archived_at !== undefined;
  const [draft, setDraft] = useState<Draft>(() => fromDetail(template));
  const [selected, setSelected] = useState<Selection>("draft");
  const [copied, setCopied] = useState<Copied | null>(null);
  const [failure, setFailure] = useState<Failure | null>(null);
  const [busy, setBusy] = useState(false);
  const [again, setAgain] = useState(0);

  const uses = useWhereUsed(workspaceId, template.id);
  const snippets = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/snippets", { params: { path: { ws: workspaceId }, query: { archived: true } } })).catch(() => []),
    [workspaceId],
  );
  const snippetNames = useMemo(() => new Map((snippets.status === "ready" ? snippets.data : []).map((snippet) => [snippet.id, snippet.name])), [snippets]);

  const changes = liveDocument === null ? 0 : diffDocuments(liveDocument, draft.document).length;
  const rows = [draftRow(live, changes, draft.savedAt), ...versions.map((version) => versionRow(version, live, authorOf(authors, version.published_by)))];
  const summary = typeof selected === "number" ? versions.find((version) => version.version === selected) : undefined;

  async function save(document: Record<string, unknown>, revision: number): Promise<Draft | null> {
    setBusy(true);
    setFailure(null);
    try {
      const saved = await unwrap(client.PUT("/api/v1/workspaces/{ws}/templates/{id}/draft", { ...idPath(workspaceId, template.id), body: { document: asBody(document), revision } }));
      const next = fromDetail(saved);
      setDraft(next);
      setAgain((value) => value + 1);
      return next;
    } catch (error) {
      setFailure({ summary: describeError(error), conflict: error instanceof ApiError && error.code === "template_draft_revision_mismatch" });
      return null;
    } finally {
      setBusy(false);
    }
  }

  async function copy(number: number) {
    const before = draft;
    setBusy(true);
    let document: Record<string, unknown>;
    try {
      document = (await unwrap(client.GET("/api/v1/workspaces/{ws}/templates/{id}/versions/{n}", { params: { path: { ws: workspaceId, id: template.id, n: number } } }))).document;
    } catch (error) {
      setFailure({ summary: describeError(error), conflict: false });
      setBusy(false);
      return;
    }
    const saved = await save(document, before.revision);
    if (saved !== null) setCopied({ version: number, before, revision: saved.revision });
  }

  async function undo() {
    if (copied === null) return;
    if ((await save(copied.before.document, copied.revision)) !== null) setCopied(null);
  }

  async function reload() {
    setBusy(true);
    try {
      setDraft(fromDetail(await unwrap(client.GET("/api/v1/workspaces/{ws}/templates/{id}", idPath(workspaceId, template.id)))));
      setCopied(null);
      setFailure(null);
      setAgain((value) => value + 1);
    } catch (error) {
      setFailure({ summary: describeError(error), conflict: false });
    } finally {
      setBusy(false);
    }
  }

  const editPath = `/w/${workspaceId}/templates/${template.id}`;
  return (
    <>
      {copied ? (
        <div className="notice hv-banner" role="status">
          <span className="row-grow">{`The draft now matches version ${copied.version}. Your earlier draft is one undo away. Nothing was published.`}</span>
          <Button onClick={() => void undo()} disabled={busy}>
            Undo
          </Button>
          <Link className="btn" to={editPath}>
            Open the draft
          </Link>
        </div>
      ) : null}
      {failure ? (
        <div className="tp-banner">
          <InlineError {...(failure.conflict ? { title: "Someone else saved the draft", detail: "Nothing was changed. Reload to see their version of the draft.", traceId: failure.summary.traceId } : failure.summary)} />
          {failure.conflict ? (
            <Button onClick={() => void reload()} disabled={busy}>
              Reload the draft
            </Button>
          ) : null}
        </div>
      ) : null}
      <div className="pb-split">
        <div className="pb-side">
          <HistoryList rows={rows} selected={selected} onSelect={setSelected} empty={versions.length === 0} />
        </div>
        <RenderCard
          workspaceId={workspaceId}
          templateId={template.id}
          selected={selected}
          canWrite={canWrite}
          canAdmin={canAdmin}
          again={again}
          action={
            typeof selected === "number" && canWrite && !archived ? (
              <Button onClick={() => void copy(selected)} disabled={busy}>
                Copy into the draft
              </Button>
            ) : selected === "draft" && canAdmin && !archived ? (
              <Link className="btn btn-primary" to={`${editPath}/publish`}>
                Review and publish
              </Link>
            ) : null
          }
        />
        <aside className="pb-side">
          <Facts
            key={String(selected)}
            workspaceId={workspaceId}
            templateId={template.id}
            selected={selected}
            draft={draft}
            author={summary ? authorOf(authors, summary.published_by) : null}
            snippetNames={snippetNames}
          />
          <UsedByPanel workspaceId={workspaceId} uses={uses} />
        </aside>
      </div>
    </>
  );
}

interface RenderProps {
  workspaceId: string;
  templateId: string;
  selected: Selection;
  canWrite: boolean;
  canAdmin: boolean;
  again: number;
  action: React.ReactNode;
}

/** The selected version as one person gets it today: the preview with `version`, rendered with today's theme and snippets. */
function RenderCard({ workspaceId, templateId, selected, canWrite, canAdmin, again, action }: RenderProps) {
  const saved = useResource(
    async () => (canAdmin ? unwrap(client.GET("/api/v1/workspaces/{ws}/templates/{id}/preview-people", idPath(workspaceId, templateId))) : []),
    [workspaceId, templateId, canAdmin],
  );
  const subjects: Subject[] = useMemo(
    () => [...(saved.status === "ready" ? saved.data.map((entry) => ({ key: entry.person_id, label: labelOf(entry.person_id, entry.display), personId: entry.person_id })) : []), SAMPLE_SUBJECT],
    [saved],
  );
  const [personKey, setPersonKey] = useState(SAMPLE_SUBJECT.key);
  const [retry, setRetry] = useState(0);
  const [limit] = useState(() => createLimiter(MAX_IN_FLIGHT));
  const subject = subjects.find((entry) => entry.key === personKey) ?? SAMPLE_SUBJECT;
  const requests = useMemo(() => (canWrite && saved.status !== "loading" ? [{ key: "render", subject, ...(typeof selected === "number" ? { version: selected } : {}) }] : null), [canWrite, saved.status, subject, selected]);
  const check = usePreviews(workspaceId, templateId, requests, limit, again + retry)["render"];
  const name = typeof selected === "number" ? `Version ${selected}` : "Draft";

  return (
    <section className="pb-render" aria-labelledby="hv-render-title">
      <div className="row pb-render-head">
        <div className="row-grow">
          <h2 className="panel-title" id="hv-render-title">{`${name} as ${subject.label} gets it`}</h2>
          <p className="faint">Rendered with today's theme and snippets, as a send would be now.</p>
        </div>
        {subjects.length > 1 ? (
          <label className="pb-person">
            <span className="visually-hidden">Show the render for</span>
            <select className="select" value={subject.key} onChange={(event) => setPersonKey(event.target.value)}>
              {subjects.map((entry) => (
                <option key={entry.key} value={entry.key}>
                  {entry.label}
                </option>
              ))}
            </select>
          </label>
        ) : null}
        {action}
      </div>
      {!canWrite ? <p className="muted">Members of the workspace can preview a template.</p> : null}
      {canWrite && saved.status === "loading" ? <LoadingState label="Loading the people to preview" /> : null}
      {canWrite && check !== undefined && check.status === "ready" ? (
        <div className="pb-subjects">
          <div>
            <span className="faint">Subject</span>
            <div className="strong pb-subject">{check.preview.subject ?? ""}</div>
          </div>
        </div>
      ) : null}
      {canWrite && saved.status !== "loading" ? <Frame title={`${name} as ${subject.label}`} check={check} marks={[]} onRetry={() => setRetry((value) => value + 1)} /> : null}
    </section>
  );
}

interface FactsProps {
  workspaceId: string;
  templateId: string;
  selected: Selection;
  draft: Draft;
  author: string | null;
  snippetNames: ReadonlyMap<string, string>;
}

/** The facts of the selected row. A version reads its document and its stats; the draft shows what the editor holds. */
function Facts({ workspaceId, templateId, selected, draft, author, snippetNames }: FactsProps) {
  const version = useResource(
    async () => (typeof selected === "number" ? unwrap(client.GET("/api/v1/workspaces/{ws}/templates/{id}/versions/{n}", { params: { path: { ws: workspaceId, id: templateId, n: selected } } })) : null),
    [workspaceId, templateId, selected],
  );
  const stats = useResource(
    async () => (typeof selected === "number" ? unwrap(client.GET("/api/v1/workspaces/{ws}/templates/{id}/versions/{n}/stats", { params: { path: { ws: workspaceId, id: templateId, n: selected } } })) : null),
    [workspaceId, templateId, selected],
  );
  const document = typeof selected === "number" ? (version.status === "ready" ? version.data?.document : undefined) : draft.document;
  const choice = document === undefined ? null : themeChoice(document);
  const themeId = choice?.kind === "named" ? choice.id : "";
  const theme = useResource(async () => (themeId === "" ? null : unwrap(client.GET("/api/v1/workspaces/{ws}/themes/{id}", { params: { path: { ws: workspaceId, id: themeId } } })).catch(() => null)), [workspaceId, themeId]);

  if (document === undefined) {
    return version.status === "loading" ? <LoadingState label="Loading the version" /> : <InlineError {...describeError(version.status === "error" ? version.error : null)} />;
  }
  return (
    <DetailsPanel
      workspaceId={workspaceId}
      title={typeof selected === "number" ? `Version ${selected}` : "Draft"}
      author={author}
      document={document}
      stats={typeof selected === "number" ? stats : null}
      themeName={theme.status === "ready" && theme.data ? theme.data.name : null}
      snippets={snippetIds(document).map((id) => ({ id, name: snippetNames.get(id) ?? "A snippet" }))}
      snippetNames={snippetNames}
    />
  );
}
