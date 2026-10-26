import { Link } from "react-router";

import { describeError, type components, type ErrorSummary, type Resource } from "../../api";
import { Button, EmptyState, ErrorState, InlineError, LoadingState } from "../../ui";
import { formatDateTime } from "../../ui/format";
import { issueBlockId } from "./blocks/document";
import type { Change } from "./diff";
import { issueText, issueWhere, type TemplateIssue, type TemplateUse, type TestSend } from "./model";
import { withOutlines } from "./outline";
import type { Check, Subject, Summary } from "./reviewChecks";
import { statusOf } from "./testSendStatus";

type SendVolume = components["schemas"]["TemplateSendVolumeBody"];

const TAG_CLASS = { Added: "pb-tag-add", Changed: "pb-tag-chg", Removed: "pb-tag-rem" } as const;

interface ChangesProps {
  live: number | null;
  changes: readonly Change[];
  focus: string | null;
  onFocus: (key: string | null) => void;
}

/** What the draft changes against the live version; selecting a row marks it in the render. */
export function ChangesPanel({ live, changes, focus, onFocus }: ChangesProps) {
  return (
    <section className="panel pb-panel" aria-labelledby="pb-changes-title">
      <h2 className="panel-title" id="pb-changes-title">
        {live === null ? "Changes" : `Changes since v${live}`}
      </h2>
      {live === null ? <p className="faint">First version</p> : null}
      {live !== null && changes.length === 0 ? <p className="faint">No changes. The draft is the same as version {live}.</p> : null}
      {changes.length > 0 ? (
        <ul className="list-plain pb-changes">
          {changes.map((change) => (
            <li key={change.key}>
              <button
                type="button"
                className={focus === change.key ? "pb-change pb-change-on" : "pb-change"}
                aria-pressed={focus === change.key}
                onClick={() => onFocus(focus === change.key ? null : change.key)}
              >
                <span className={`pb-tag ${TAG_CLASS[change.tag]}`}>{change.tag}</span>
                <span className="pb-change-text">
                  <span className="strong">{change.where}</span>
                  <span className="faint">{change.what}</span>
                </span>
              </button>
            </li>
          ))}
        </ul>
      ) : null}
    </section>
  );
}

interface ChecksProps {
  workspaceId: string;
  templateId: string;
  subjects: readonly Subject[];
  summary: Summary;
  onRetry: () => void;
  /** Set when the saved people could not be loaded: only the sample person was checked. */
  people: { error: ErrorSummary; onRetry: () => void } | null;
}

/** The fix link of an issue: the editor, at the block when the issue names one. */
function fixTo(workspaceId: string, templateId: string, issue: TemplateIssue): string {
  const block = issueBlockId(issue.at);
  const base = `/w/${workspaceId}/templates/${templateId}`;
  return block === null ? base : `${base}?block=${encodeURIComponent(block)}`;
}

function IssueRow({ workspaceId, templateId, issue }: { workspaceId: string; templateId: string; issue: TemplateIssue }) {
  const blocking = issue.severity === "blocking";
  return (
    <li className="pb-issue">
      <span className={blocking ? "pb-dot pb-dot-block" : "pb-dot pb-dot-warn"} aria-hidden="true" />
      <span className="pb-issue-text">
        <span className="strong">{issueText(issue)}</span>
        <span className="faint">
          {issueWhere(issue)}. {blocking ? "Blocks publishing." : "You can publish with a warning."}
        </span>
      </span>
      <Link className="btn" to={fixTo(workspaceId, templateId, issue)}>
        Fix
      </Link>
    </li>
  );
}

/** The checks across the saved preview people and the sample person. */
export function ChecksPanel({ workspaceId, templateId, subjects, summary, onRetry, people }: ChecksProps) {
  const names = subjects.map((subject) => subject.label);
  const list = names.length > 1 ? `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}` : (names[0] ?? "");
  return (
    <section className="panel pb-panel" aria-labelledby="pb-checks-title">
      <h2 className="panel-title" id="pb-checks-title">
        Checks
      </h2>
      <p className="faint">For {list}</p>
      {!summary.done ? <LoadingState label="Checking the draft" /> : null}
      {people ? (
        <div className="stack-tight">
          <InlineError title="The saved people could not be loaded" detail={people.error.detail || people.error.title} traceId={people.error.traceId} />
          <Button onClick={people.onRetry}>Retry</Button>
        </div>
      ) : null}
      {summary.done && summary.failed > 0 ? (
        <div className="stack-tight">
          <InlineError title="Some checks could not run" detail="Publishing waits until every check has run." traceId={null} />
          <Button onClick={onRetry}>Check again</Button>
        </div>
      ) : null}
      {summary.done && summary.failed === 0 && summary.blocking.length === 0 && !people ? (
        <p className="pb-ok" role="status">
          Nothing blocks publishing. {summary.passed} {summary.passed === 1 ? "check" : "checks"} passed.
        </p>
      ) : null}
      {summary.done && summary.blocking.length > 0 ? (
        <p className="pb-bad" role="status">
          {summary.blocking.length} blocking {summary.blocking.length === 1 ? "issue" : "issues"} must be fixed first.
        </p>
      ) : null}
      {summary.blocking.length + summary.warnings.length > 0 ? (
        <ul className="list-plain pb-issues" aria-label="Issues found">
          {[...summary.blocking, ...summary.warnings].map((issue) => (
            <IssueRow key={`${issue.code}|${issue.at}|${issue.field ?? ""}`} workspaceId={workspaceId} templateId={templateId} issue={issue} />
          ))}
        </ul>
      ) : null}
    </section>
  );
}

function perDay(volume: SendVolume | undefined): string | null {
  if (volume === undefined) return null;
  if (volume.per_day <= 0) return "no sends in the last 7 days";
  if (volume.per_day < 1) return "fewer than 1 a day";
  return `about ${Math.round(volume.per_day).toLocaleString("en")} a day`;
}

interface LiveProps {
  workspaceId: string;
  uses: Resource<TemplateUse[]>;
  volume: Resource<SendVolume[]>;
}

/** The active flows that send the template, with how much each sends. */
export function GoesLivePanel({ workspaceId, uses, volume }: LiveProps) {
  const sends = new Map(volume.status === "ready" ? volume.data.map((entry) => [entry.flow_id, entry]) : []);
  const active = uses.status === "ready" ? uses.data.filter((use) => use.active && use.flow_state !== "archived") : [];
  return (
    <section className="panel pb-panel" aria-labelledby="pb-live-title">
      <h2 className="panel-title" id="pb-live-title">
        Where it goes live
      </h2>
      <p className="faint">Flows send a template's newest published version. No flow needs republishing.</p>
      {uses.status === "loading" ? <LoadingState label="Checking which flows use this template" /> : null}
      {uses.status === "error" ? <ErrorState {...describeError(uses.error)} onRetry={uses.reload} /> : null}
      {uses.status === "ready" && active.length === 0 ? <p className="faint">No active flow uses this template yet.</p> : null}
      {active.length > 0 && volume.status === "loading" ? <LoadingState label="Counting recent sends" /> : null}
      {active.length > 0 && volume.status === "error" ? (
        <div className="row">
          <InlineError title="Sends a day could not be loaded" detail={describeError(volume.error).detail} traceId={null} />
          <Button onClick={volume.reload}>Retry</Button>
        </div>
      ) : null}
      {active.length > 0 ? (
        <ul className="list-plain pb-flows" aria-label="Flows that send this template">
          {active.map((use) => (
            <li key={use.flow_id}>
              <Link className="pb-flow" to={`/w/${workspaceId}/flows/${use.flow_id}`}>
                <span className="strong">{use.flow_name}</span>
                <span className="faint">{volume.status === "ready" ? (perDay(sends.get(use.flow_id)) ?? use.flow_state) : use.flow_state}</span>
              </Link>
            </li>
          ))}
        </ul>
      ) : null}
    </section>
  );
}

interface TestProps {
  workspaceId: string;
  templateId: string;
  log: Resource<{ test_sends: TestSend[] }>;
}

/** The newest test of this draft, or a nudge to send one. */
export function LastTestPanel({ workspaceId, templateId, log }: TestProps) {
  const newest = log.status === "ready" ? log.data.test_sends[0] : undefined;
  return (
    <section className="panel pb-panel" aria-labelledby="pb-test-title">
      <div className="row">
        <h2 className="panel-title row-grow" id="pb-test-title">
          {newest ? `Tested this draft at ${formatDateTime(newest.created_at)}` : "Not tested yet"}
        </h2>
        <Link className="btn" to={`/w/${workspaceId}/templates/${templateId}/test`}>
          Send a test
        </Link>
      </div>
      {log.status === "loading" ? <LoadingState label="Looking for the last test" /> : null}
      {log.status === "error" ? <ErrorState {...describeError(log.error)} onRetry={log.reload} /> : null}
      {newest ? (
        <p className="faint">
          To {newest.recipient_display} as {newest.render_context === "person" ? "a saved person" : "the sample person"} · {statusOf(newest).label.toLowerCase()}
        </p>
      ) : null}
      {log.status === "ready" && !newest ? <p className="faint">No test has been sent of this version of the draft.</p> : null}
    </section>
  );
}

interface RenderProps {
  subject: Subject;
  subjects: readonly Subject[];
  onSubject: (key: string) => void;
  live: number | null;
  next: number;
  compare: "side" | "draft";
  onCompare: (compare: "side" | "draft") => void;
  draft: Check | undefined;
  before: Check | undefined;
  changes: readonly Change[];
  onRetry: () => void;
}

function Frame({ title, check, marks, onRetry }: { title: string; check: Check | undefined; marks: readonly Change[]; onRetry: () => void }) {
  if (check === undefined || check.status === "running") return <LoadingState label="Rendering" />;
  if (check.status === "failed") return <ErrorState title="The render failed" detail={check.error.detail || check.error.title} traceId={check.error.traceId} onRetry={onRetry} />;
  const { preview } = check;
  if (!preview.rendered) return <EmptyState title="Nothing to show">This version has blocking issues, so it is not rendered.</EmptyState>;
  if (preview.html == null) return <pre className="fl-code tp-text">{preview.body ?? ""}</pre>;
  return (
    <div className="tp-frame">
      <iframe title={title} sandbox="" srcDoc={withOutlines(preview.html, marks)} className="tp-iframe" />
    </div>
  );
}

/** The side-by-side render of the live version and the draft, with the changed blocks outlined. */
export function RenderPanel({ subject, subjects, onSubject, live, next, compare, onCompare, draft, before, changes, onRetry }: RenderProps) {
  const side = live !== null && compare === "side";
  const subjectOf = (check: Check | undefined) => (check?.status === "ready" ? (check.preview.subject ?? "") : "");
  return (
    <section className="panel pb-render" aria-labelledby="pb-render-title">
      <div className="row pb-render-head">
        <h2 className="panel-title" id="pb-render-title">
          What {subject.label} gets
        </h2>
        {subjects.length > 1 ? (
          <label className="pb-person">
            <span className="visually-hidden">Show the render for</span>
            <select className="select" value={subject.key} onChange={(event) => onSubject(event.target.value)}>
              {subjects.map((entry) => (
                <option key={entry.key} value={entry.key}>
                  {entry.label}
                </option>
              ))}
            </select>
          </label>
        ) : null}
        {live !== null ? (
          <div className="seg-tabs pb-compare" role="group" aria-label="Compare">
            <button type="button" className={compare === "side" ? "seg seg-on" : "seg"} aria-pressed={compare === "side"} onClick={() => onCompare("side")}>
              {`v${live} and v${next}`}
            </button>
            <button type="button" className={compare === "draft" ? "seg seg-on" : "seg"} aria-pressed={compare === "draft"} onClick={() => onCompare("draft")}>
              {`v${next} only`}
            </button>
          </div>
        ) : null}
        <span className="pb-legend faint">
          <span className="pb-dot pb-dot-add" aria-hidden="true" /> Added <span className="pb-dot pb-dot-chg" aria-hidden="true" /> Changed
        </span>
      </div>
      <div className={side ? "pb-subjects pb-cols-2" : "pb-subjects"}>
        {side ? (
          <div>
            <span className="faint">{`v${live} subject`}</span>
            <div className="strong pb-subject">{subjectOf(before)}</div>
          </div>
        ) : null}
        <div>
          <span className="faint">{`v${next} subject`}</span>
          <div className="strong pb-subject">{subjectOf(draft)}</div>
        </div>
      </div>
      <div className={side ? "pb-frames pb-cols-2" : "pb-frames"}>
        {side ? (
          <div className="pb-frame">
            <p className="field-label">{`v${live} · live`}</p>
            <Frame title={`Version ${live}`} check={before} marks={changes.filter((change) => change.tag === "Removed")} onRetry={onRetry} />
          </div>
        ) : null}
        <div className="pb-frame">
          <p className="field-label">{`v${next} · draft`}</p>
          <Frame title={`Version ${next} draft`} check={draft} marks={changes.filter((change) => change.tag !== "Removed")} onRetry={onRetry} />
        </div>
      </div>
    </section>
  );
}
