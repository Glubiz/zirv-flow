import { useMemo, useState } from "react";
import { Link, useParams } from "react-router";

import { ApiError, client, describeError, unwrap, useResource, type components, type ErrorSummary } from "../../api";
import { Button, EmptyState, InlineError, ResourceView, TextField } from "../../ui";
import { useTemplateAccess } from "./access";
import { diffDocuments } from "./diff";
import type { Template, TemplateDetail, TemplateVersion } from "./model";
import { labelOf } from "./PreviewPane";
import { ChangesPanel, ChecksPanel, GoesLivePanel, LastTestPanel, RenderPanel } from "./PublishPanels";
import { createLimiter, MAX_IN_FLIGHT, SAMPLE_SUBJECT, summarize, type Subject } from "./reviewChecks";
import { usePreviews } from "./usePreviews";
import { useWhereUsed } from "./WhereUsed";

type Published = components["schemas"]["TemplatePublishedBody"];

const NOTE_MAX = 500;

export function PublishPage() {
  const { ws = "", id = "" } = useParams();
  const access = useTemplateAccess(ws);
  const detail = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/templates/{id}", { params: { path: { ws, id } } })),
    [ws, id],
  );
  return (
    <ResourceView resource={access.ready && !(detail.status === "ready" && detail.refreshing) ? detail : { status: "loading", reload: detail.reload }} loadingLabel="Loading the template">
      {(data) => <Gate key={`${ws}:${id}`} workspaceId={ws} template={data} canWrite={access.canWrite} canAdmin={access.canAdmin} onReload={detail.reload} />}
    </ResourceView>
  );
}

interface GateProps {
  workspaceId: string;
  template: TemplateDetail;
  canWrite: boolean;
  canAdmin: boolean;
  /** Reads the template again, which starts the review over on the draft as it is now. */
  onReload: () => void;
}

function Gate(props: GateProps) {
  const { workspaceId, template, canWrite } = props;
  const archived = template.archived_at !== null && template.archived_at !== undefined;
  const reason = !canWrite ? "Members of the workspace can review a template." : archived ? "This template is archived, so it cannot be published." : null;
  if (reason !== null) {
    return (
      <div className="tp-page">
        <Link to={`/w/${workspaceId}/templates/${template.id}`}>← Back to editing</Link>
        <EmptyState title="Nothing to review">{reason}</EmptyState>
      </div>
    );
  }
  return <WithLive {...props} />;
}

/** Reads the live version's document, which the diff compares the draft with. */
function WithLive(props: GateProps) {
  const { workspaceId, template } = props;
  const number = template.current_version ?? null;
  const live = useResource(
    async (): Promise<TemplateVersion | null> =>
      number === null
        ? null
        : unwrap(client.GET("/api/v1/workspaces/{ws}/templates/{id}/versions/{n}", { params: { path: { ws: workspaceId, id: template.id, n: number } } })),
    [workspaceId, template.id, number],
  );
  return (
    <ResourceView resource={live} loadingLabel="Loading the live version">
      {(version) => <Review key={props.template.draft.revision} {...props} live={version} />}
    </ResourceView>
  );
}

function Review({ workspaceId, template, canAdmin, live, onReload }: GateProps & { live: TemplateVersion | null }) {
  const liveNumber = live?.version ?? null;
  const next = (liveNumber ?? 0) + 1;
  const saved = useResource(
    async () => (canAdmin ? unwrap(client.GET("/api/v1/workspaces/{ws}/templates/{id}/preview-people", { params: { path: { ws: workspaceId, id: template.id } } })) : []),
    [workspaceId, template.id, canAdmin],
  );
  const uses = useWhereUsed(workspaceId, template.id);
  const volume = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/templates/{id}/send-volume", { params: { path: { ws: workspaceId, id: template.id } } })),
    [workspaceId, template.id],
  );
  const log = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/test-sends", {
          params: { path: { ws: workspaceId }, query: { template_id: template.id, draft_revision: template.draft.revision, limit: 1 } },
        }),
      ),
    [workspaceId, template.id, template.draft.revision],
  );

  const savedPeople = saved.status === "ready" ? saved.data : null;
  const savedFailed = saved.status === "error";
  const subjects: Subject[] | null = useMemo(
    () =>
      savedPeople !== null
        ? [...savedPeople.map((entry) => ({ key: entry.person_id, label: labelOf(entry.person_id, entry.display), personId: entry.person_id })), SAMPLE_SUBJECT]
        : savedFailed
          ? [SAMPLE_SUBJECT]
          : null,
    [savedPeople, savedFailed],
  );

  const [again, setAgain] = useState(0);
  const [personKey, setPersonKey] = useState(SAMPLE_SUBJECT.key);
  const [compare, setCompare] = useState<"side" | "draft">("side");
  const [focus, setFocus] = useState<string | null>(null);
  const [stale, setStale] = useState(false);
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [published, setPublished] = useState<Published | null>(null);
  /** Every preview of the page goes through one limiter, so three at most are in flight. */
  const [limit] = useState(() => createLimiter(MAX_IN_FLIGHT));

  const subject = subjects?.find((entry) => entry.key === personKey) ?? SAMPLE_SUBJECT;
  const sideBySide = liveNumber !== null && compare === "side";
  const draftRequests = useMemo(() => subjects?.map((entry) => ({ key: entry.key, subject: entry })) ?? null, [subjects]);
  const liveRequests = useMemo(() => (sideBySide ? [{ key: subject.key, subject, version: liveNumber }] : null), [sideBySide, subject, liveNumber]);
  const checks = usePreviews(workspaceId, template.id, draftRequests, limit, again);
  const beforeChecks = usePreviews(workspaceId, template.id, liveRequests, limit, again);

  const changes = useMemo(() => (live === null ? [] : diffDocuments(live.document, template.draft.document)), [live, template.draft.document]);
  const shown = focus === null ? changes : changes.filter((change) => change.key === focus);
  const summary = summarize(subjects ?? [], checks);
  const peopleError: ErrorSummary | null = saved.status === "error" ? describeError(saved.error) : null;
  const ready = subjects !== null && summary.done && summary.failed === 0 && summary.blocking.length === 0 && peopleError === null;

  async function publish() {
    setBusy(true);
    setFailure(null);
    try {
      const trimmed = note.trim();
      const result = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/templates/{id}/publish", {
          params: { path: { ws: workspaceId, id: template.id } },
          body: { revision: template.draft.revision, ...(trimmed === "" ? {} : { note: trimmed }) },
        }),
      );
      setPublished(result);
    } catch (error) {
      if (error instanceof ApiError && error.code === "template_draft_changed") setStale(true);
      else setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  const heading = published ? `Version ${published.version.version} is live` : `Review and publish version ${next}`;
  const why = !canAdmin ? "Only workspace admins can publish." : peopleError !== null ? "The saved people could not be loaded." : subjects === null || !summary.done ? "Waiting for the checks." : summary.failed > 0 ? "A check could not run." : summary.blocking.length > 0 ? "Fix the blocking issues first." : null;
  const draftCheck = checks[subject.key];

  return (
    <div className="tp-page pb-page">
      <div className="tp-head">
        <Link to={`/w/${workspaceId}/templates/${template.id}`}>← Back to editing</Link>
        <div className="row-grow">
          <p className="faint">{template.name}</p>
          <h1 className="fl-name-read">{heading}</h1>
        </div>
        {!published ? (
          <>
            {liveNumber !== null ? <span className="faint">{`v${liveNumber} stays live until you publish`}</span> : null}
            <Button variant="primary" onClick={() => void publish()} disabled={busy || !ready || !canAdmin} aria-describedby={why ? "pb-why" : undefined}>
              {busy ? "Publishing" : `Publish version ${next}`}
            </Button>
          </>
        ) : null}
      </div>
      {!published && why ? (
        <p id="pb-why" className="faint tp-why">
          {why}
        </p>
      ) : null}
      <div className="pb-split">
        <div className="pb-side">
          <ChangesPanel live={liveNumber} changes={changes} focus={focus} onFocus={setFocus} />
          <ChecksPanel workspaceId={workspaceId} templateId={template.id} subjects={subjects ?? [SAMPLE_SUBJECT]} summary={subjects === null ? { ...summary, done: false } : summary} onRetry={() => setAgain((value) => value + 1)} people={peopleError === null ? null : { error: peopleError, onRetry: saved.reload }} />
          <GoesLivePanel workspaceId={workspaceId} uses={uses} volume={volume} />
          <LastTestPanel workspaceId={workspaceId} templateId={template.id} log={log} />
          {published ? (
            <Done workspaceId={workspaceId} template={template} published={published} />
          ) : (
            <section className="panel pb-panel pb-publish" aria-label="Publish">
              <TextField
                label="Note for the version history"
                hint="Optional. Shown with the version in its history."
                value={note}
                maxLength={NOTE_MAX}
                onChange={(event) => setNote(event.target.value)}
              />
              {failure ? <InlineError {...failure} /> : null}
              {stale ? (
                <div className="stack-tight" role="alert">
                  <InlineError title="The draft changed while you were reviewing it." detail="Nothing was published. Reload to review the draft as it is now." traceId={null} />
                  <Button onClick={onReload}>Reload</Button>
                </div>
              ) : null}
              <div className="row">
                <Button variant="primary" onClick={() => void publish()} disabled={busy || !ready || !canAdmin}>
                  {busy ? "Publishing" : `Publish version ${next}`}
                </Button>
                <Link className="btn" to={`/w/${workspaceId}/templates/${template.id}`}>
                  Keep editing
                </Link>
              </div>
              {liveNumber !== null ? <p className="faint">{`Version ${liveNumber} stays in the history. You can copy it back into the draft at any time.`}</p> : null}
            </section>
          )}
        </div>
        <RenderPanel
          subject={subject}
          subjects={subjects ?? [SAMPLE_SUBJECT]}
          onSubject={setPersonKey}
          live={liveNumber}
          next={next}
          compare={compare}
          onCompare={setCompare}
          draft={draftCheck}
          before={beforeChecks[subject.key]}
          changes={shown}
          onRetry={() => setAgain((value) => value + 1)}
        />
      </div>
    </div>
  );
}

function Done({ workspaceId, template, published }: { workspaceId: string; template: Template; published: Published }) {
  const flows = published.used_by;
  return (
    <section className="panel pb-panel pb-done" role="status" aria-label="Published">
      <h2 className="panel-title">{`Version ${published.version.version} is live`}</h2>
      {flows.length === 0 ? (
        <p>No active flow sends this template yet. The first one to will use it.</p>
      ) : (
        <>
          <p>The next send in these flows uses it. Messages already queued keep the version they started with.</p>
          <ul className="list-plain" aria-label="Flows that pick it up">
            {flows.map((flow) => (
              <li key={flow.flow_id}>
                <Link to={`/w/${workspaceId}/flows/${flow.flow_id}`}>{flow.flow_name}</Link>
              </li>
            ))}
          </ul>
        </>
      )}
      <div className="row">
        <Link className="btn" to={`/w/${workspaceId}/templates/${template.id}`}>
          View history
        </Link>
        <Link className="btn" to={`/w/${workspaceId}/templates`}>
          Back to the library
        </Link>
      </div>
    </section>
  );
}
