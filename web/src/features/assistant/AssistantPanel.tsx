import { useEffect, useRef, useState, type ReactNode } from "react";
import { Link, useInRouterContext } from "react-router";

import { client, unwrap, useResource } from "../../api";
import { Button, EmptyState, InlineError, LoadingState, ResourceView, TextAreaField } from "../../ui";
import { DraftChanges } from "./DraftChanges";
import { DryRunResult } from "./DryRunResult";
import { failureText, issueSentence, type Draft, type DraftKindName, type Failure } from "./labels";

/** The longest prompt the server takes, in characters. */
export const MAX_PROMPT_CHARS = 2000;
/** The most documentation the server takes, in bytes. */
export const MAX_DOCUMENTATION_BYTES = 32 * 1024;

type Run =
  | { status: "idle" }
  | { status: "running" }
  | { status: "cancelled" }
  | { status: "failed"; failure: Failure }
  | { status: "done"; draft: Draft; accepting: boolean; accepted: boolean; acceptFailure: Failure | null };

interface AssistantPanelProps {
  workspaceId: string;
  kind: DraftKindName;
  /** The source: required for a mapping; for a manifest, the saved `http` source being edited. */
  sourceId?: string;
  /** The stream whose mapping is drafted. */
  stream?: string;
  /** For a segment: the saved segment being edited; absent when the segment is new. */
  segmentId?: string;
  /** For a flow: the flow whose draft is being edited. */
  flowId?: string;
  /**
   * For a webhook stream, which has no source to read: the parsed sample payload, or null while the
   * pasted text does not parse. Absent for any other stream.
   */
  sample?: unknown[] | null;
  /** Called with the validated draft once the person accepts it. It loads the editor's unsaved copy; saving stays theirs. */
  onAccept: (content: unknown) => void;
  /**
   * Asked before the accept request, so a refusal accepts nothing on the server: the host returns
   * false when loading the draft would replace edits the person has not agreed to lose. It may ask
   * the person in a dialog of its own and answer when they have.
   */
  confirmAccept?: () => boolean | Promise<boolean>;
  /**
   * Called when the accept request failed after `confirmAccept` agreed, so the host can undo what it prepared.
   * It may answer true when it found that the draft was accepted after all and its reply was lost.
   */
  onAcceptFailed?: () => unknown;
  /** Called with the error of a request that failed, so the host can tell that the assistant has been turned off. */
  onFailure?: (error: unknown) => void;
  /** Replaces the field table of the changes, for a kind whose draft is better read in the host's own words. */
  renderChanges?: (changes: Draft["changes"], draft: Draft) => ReactNode;
  /**
   * Shows what the drafted content makes of the workspace's data, for a kind whose draft has no dry
   * run on the server. The host renders it, so this feature never imports the host. It also gets the
   * whole draft, for what only a draft carries, such as a flow's simulation.
   */
  renderPreview?: (content: unknown, draft: Draft) => ReactNode;
}

/**
 * The assistant of the mapping dialog and the manifest editor. The person says what they want,
 * the server drafts and validates it, and the panel shows the diff and the dry run. A draft that
 * did not validate has no way to be accepted.
 */
export function AssistantPanel(props: AssistantPanelProps) {
  const settings = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/ai/settings", { params: { path: { ws: props.workspaceId } } })),
    [props.workspaceId],
  );
  return (
    <section className="stack-tight" aria-label="Assistant">
      <h3>Draft with the assistant</h3>
      <ResourceView resource={settings} loadingLabel="Checking the assistant">
        {(current) =>
          current.enabled ? (
            <Drafting {...props} personal={current.pii_opt_in} />
          ) : (
            <EmptyState title="The assistant is off">
              An admin can turn it on in the <AiSettingsLink workspaceId={props.workspaceId} />.
            </EmptyState>
          )
        }
      </ResourceView>
    </section>
  );
}

function AiSettingsLink({ workspaceId }: { workspaceId: string }) {
  const inRouter = useInRouterContext();
  return inRouter ? <Link to={`/w/${workspaceId}/ai`}>AI settings</Link> : <>AI settings</>;
}

function byteLength(text: string): number {
  return new TextEncoder().encode(text).length;
}

function plural(count: number, one: string, many: string): string {
  return `${count} ${count === 1 ? one : many}`;
}

function Drafting({
  workspaceId,
  kind,
  sourceId,
  stream,
  segmentId,
  flowId,
  sample,
  onAccept,
  confirmAccept,
  onAcceptFailed,
  onFailure,
  renderChanges,
  renderPreview,
  personal,
}: AssistantPanelProps & { personal: boolean }) {
  const [prompt, setPrompt] = useState("");
  const [documentation, setDocumentation] = useState("");
  const [run, setRun] = useState<Run>({ status: "idle" });
  const [announcement, setAnnouncement] = useState("");
  const controller = useRef<AbortController | null>(null);
  const heading = useRef<HTMLHeadingElement>(null);
  const accepting = useRef(false);
  const [lateReply, setLateReply] = useState(false);

  const doneId = run.status === "done" ? run.draft.id : null;
  useEffect(() => {
    if (doneId !== null) heading.current?.focus();
  }, [doneId]);
  useEffect(() => () => controller.current?.abort(), []);

  const waitingForSample = sample === null;
  const tooLong = prompt.length > MAX_PROMPT_CHARS || byteLength(documentation) > MAX_DOCUMENTATION_BYTES;
  const canDraft = run.status !== "running" && prompt.trim() !== "" && !tooLong && !waitingForSample;

  async function draft() {
    if (!canDraft) return;
    const abort = new AbortController();
    controller.current = abort;
    setRun({ status: "running" });
    setLateReply(false);
    setAnnouncement("Drafting");
    try {
      const result = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/ai/drafts", {
          params: { path: { ws: workspaceId } },
          body: {
            kind,
            prompt: prompt.trim(),
            ...(sourceId ? { source_id: sourceId } : {}),
            ...(kind === "mapping" && stream ? { stream } : {}),
            ...(kind === "segment" && segmentId ? { segment_id: segmentId } : {}),
            ...(kind === "flow" && flowId ? { flow_id: flowId } : {}),
            ...(kind === "manifest" && documentation.trim() !== "" ? { documentation } : {}),
            ...(sample ? { sample: sample as never } : {}),
          },
          signal: abort.signal,
        }),
      );
      setRun({ status: "done", draft: result, accepting: false, accepted: false, acceptFailure: null });
      setAnnouncement(result.status === "valid" ? "Draft ready" : "No valid draft");
    } catch (error) {
      if (abort.signal.aborted) {
        setRun({ status: "cancelled" });
        setAnnouncement("Cancelled");
      } else {
        onFailure?.(error);
        setRun({ status: "failed", failure: failureText(error) });
        setAnnouncement("Drafting failed");
      }
    }
  }

  async function accept(current: Draft) {
    // A second click while the first is still being confirmed or sent would send a second request, which is refused.
    if (accepting.current) return;
    accepting.current = true;
    try {
      await acceptOnce(current);
    } finally {
      accepting.current = false;
    }
  }

  async function acceptOnce(current: Draft) {
    if (confirmAccept && !(await confirmAccept())) return;
    setRun({ status: "done", draft: current, accepting: true, accepted: false, acceptFailure: null });
    try {
      const accepted = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/ai/drafts/{id}/accept", {
          params: { path: { ws: workspaceId, id: current.id } },
        }),
      );
      onAccept(accepted.content);
      setRun({ status: "done", draft: current, accepting: false, accepted: true, acceptFailure: null });
      setAnnouncement("Loaded into the editor");
    } catch (error) {
      if ((await onAcceptFailed?.()) === true) {
        setRun({ status: "done", draft: current, accepting: false, accepted: true, acceptFailure: null });
        setLateReply(true);
        setAnnouncement("The draft was accepted, but the reply was lost. It is loaded into the editor.");
        return;
      }
      onFailure?.(error);
      setRun({ status: "done", draft: current, accepting: false, accepted: false, acceptFailure: failureText(error) });
    }
  }

  return (
    <div className="stack-tight">
      <p className="faint">
        {kind === "flow"
          ? personal
            ? "Sends the steps and settings of the saved draft and the names of your fields, events and lists (personal data is allowed in this workspace)."
            : "Sends the steps and settings of the saved draft and the names of your fields, events and lists. Values that could be personal are hidden from it."
          : kind === "segment"
          ? personal
            ? "Sends the names of your fields, events and lists, and the values of the saved definition (personal data is allowed in this workspace)."
            : "Sends the names of your fields, events and lists. Values in the saved definition that could be personal are hidden from it."
          : personal
            ? "Also sends up to 5 sample records (personal data is allowed in this workspace)."
            : "Sends field names and types only."}
      </p>
      <TextAreaField
        label="What do you want?"
        hint={`${prompt.length} of ${MAX_PROMPT_CHARS} characters. Do not paste personal data here.`}
        rows={3}
        value={prompt}
        disabled={run.status === "running"}
        onChange={(event) => setPrompt(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) {
            event.preventDefault();
            void draft();
          }
        }}
      />
      {kind === "manifest" ? (
        <>
          <TextAreaField
            label="API documentation"
            hint={`Paste the part of the documentation that describes the endpoints. ${Math.ceil(byteLength(documentation) / 1024)} of 32 KiB. The assistant reads it as data, not as instructions.`}
            rows={6}
            mono
            value={documentation}
            disabled={run.status === "running"}
            onChange={(event) => setDocumentation(event.target.value)}
          />
          {sourceId ? null : (
            <p className="faint">
              The source is not saved yet, so the draft cannot be tried against the API. Save the source to see a sample.
            </p>
          )}
        </>
      ) : null}
      {waitingForSample ? <p className="faint">Paste a sample payload that parses first: its fields are what the assistant maps.</p> : null}
      <div className="row">
        <Button variant="primary" onClick={() => void draft()} disabled={!canDraft}>
          Draft
        </Button>
        {run.status === "running" ? <Button onClick={() => controller.current?.abort()}>Cancel</Button> : null}
      </div>
      <p className="visually-hidden" role="status">
        {announcement}
      </p>
      {run.status === "running" ? <LoadingState label="Drafting, this can take a minute" /> : null}
      {run.status === "cancelled" ? <p className="faint">Cancelled. Nothing was kept.</p> : null}
      {run.status === "failed" ? <InlineError {...run.failure} /> : null}
      {run.status === "done" ? (
        run.draft.status === "valid" ? (
          <div className="stack-tight">
            <h4 ref={heading} tabIndex={-1}>
              Draft ready
            </h4>
            <p className="faint">
              {run.draft.attempts === 1 ? "Valid on the first try." : `Valid after ${plural(run.draft.attempts, "attempt", "attempts")}.`}
            </p>
            <h5>Changes</h5>
            {renderChanges ? renderChanges(run.draft.changes, run.draft) : <DraftChanges changes={run.draft.changes} />}
            {renderPreview && run.draft.content != null ? (
              <>
                <h5>Preview</h5>
                {renderPreview(run.draft.content, run.draft)}
              </>
            ) : null}
            {run.draft.dry_run ? (
              <>
                <h5>Dry run</h5>
                <DryRunResult dryRun={run.draft.dry_run} />
              </>
            ) : null}
            {run.acceptFailure ? <InlineError {...run.acceptFailure} /> : null}
            {run.accepted && lateReply ? <p className="strong">The draft was accepted, but the reply was lost. It is loaded into the editor.</p> : null}
            {run.accepted ? (
              <p className="strong">
                {kind === "flow"
                  ? "Loaded into the editor and saved as the flow's draft. Nothing is published."
                  : "Loaded into the editor. Save to apply it."}
              </p>
            ) : (
              <div className="row">
                <Button variant="primary" onClick={() => void accept(run.draft)} disabled={run.accepting}>
                  {run.accepting ? "Accepting" : "Accept"}
                </Button>
                <Button onClick={() => setRun({ status: "idle" })} disabled={run.accepting}>
                  Discard
                </Button>
              </div>
            )}
          </div>
        ) : (
          <div className="stack-tight">
            <h4 ref={heading} tabIndex={-1}>
              {`No valid draft after ${plural(run.draft.attempts, "attempt", "attempts")}`}
            </h4>
            <ul className="list-plain attention" aria-label="Problems with the last attempt">
              {run.draft.issues.map((issue, index) => (
                <li key={`${issue.code}-${issue.at ?? ""}-${index}`}>{issueSentence(issue, kind)}</li>
              ))}
            </ul>
            <p className="faint">Say it differently, or add detail, and draft again.</p>
          </div>
        )
      ) : null}
    </div>
  );
}
