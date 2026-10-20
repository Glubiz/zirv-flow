import { useCallback, useEffect, useId, useRef, useState, type ReactNode } from "react";

import { client, describeError, unwrap, type components, type ErrorSummary } from "../../api";
import { Button, Dialog, formatAgo, LoadingState } from "../../ui";
import type { IssueItem } from "./Canvas";
import type { Names } from "./describe";
import { issueText } from "./issues";
import type { FlowIssue, Graph } from "./model";

type Impact = components["schemas"]["ImpactBody"];

interface PublishDialogProps {
  workspaceId: string;
  flowId: string;
  flowName: string;
  graph: Graph;
  names: Names;
  /** Saves what the editor holds; false when the save failed. */
  saveNow: () => Promise<boolean>;
  /** The issues as entries with the step or section each one is about. */
  itemsFor: (issues: FlowIssue[]) => IssueItem[];
  /** Closes the dialog and shows the step or section of the issue. */
  onPick: (item: IssueItem) => void;
  /** Whether the editor has a publish request in flight; the editor owns it, so closing the dialog does not stop it. */
  publishing: boolean;
  /** Starts the publish; the editor handles the outcome whether or not this dialog is still open. */
  onPublish: () => void;
  /** Why the last publish failed, when it did for a reason other than the flow's issues. */
  refusal: ErrorSummary | null;
  /** The issues a publish was refused with (a 422), so the checks list shows them. */
  refusedIssues: FlowIssue[] | null;
  onClose: () => void;
}

type Phase = { name: "loading" } | { name: "failed"; error: ErrorSummary } | { name: "ready" };

function people(count: number): string {
  return `${count.toLocaleString("en")} ${count === 1 ? "person" : "people"}`;
}

function Row({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="pub-row">
      <div className="pub-label">{label}</div>
      <div className="pub-body">{children}</div>
    </div>
  );
}

/** The preview of a publish, from a dry run, and the publish itself. */
export function PublishDialog(props: PublishDialogProps) {
  const { workspaceId, flowId, flowName, graph, names, saveNow, itemsFor, onPick, publishing, onPublish, refusal, refusedIssues, onClose } = props;
  const [phase, setPhase] = useState<Phase>({ name: "loading" });
  const [dryRun, setDryRun] = useState<Impact | null>(null);
  /** The dry run, with the issues a refused publish found laid over it. */
  const impact = dryRun && refusedIssues ? { ...dryRun, valid: false, issues: refusedIssues } : dryRun;
  const reasonId = useId();
  const closeId = useId();
  const live = useRef(true);
  /** Whether the dialog is still open; read through a function so a check after an await is not narrowed away. */
  const alive = () => live.current;
  /** The newest call of `saveNow`, kept so Retry does not restart the effect that loads. */
  const save = useRef(saveNow);

  useEffect(() => {
    save.current = saveNow;
  });

  const load = useCallback(async () => {
    setPhase({ name: "loading" });
    try {
      const saved = await save.current();
      if (!alive()) return;
      if (!saved) {
        setPhase({
          name: "failed",
          error: {
            title: "Couldn’t save your changes first",
            detail: "Publishing uses the saved flow. Try again once it saves.",
            traceId: null,
          },
        });
        return;
      }
      const body = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/flows/{id}/publish", {
          params: { path: { ws: workspaceId, id: flowId }, query: { dry_run: true } },
        }),
      );
      if (!alive()) return;
      setDryRun(body as Impact);
      setPhase({ name: "ready" });
    } catch (error) {
      if (alive()) setPhase({ name: "failed", error: describeError(error) });
    }
  }, [workspaceId, flowId]);

  useEffect(() => {
    live.current = true;
    const timeout = setTimeout(() => void load(), 0);
    return () => {
      live.current = false;
      clearTimeout(timeout);
    };
  }, [load]);

  /** The button that started the publish is disabled now, which would drop focus out of the dialog: Close takes it. */
  useEffect(() => {
    if (publishing) document.getElementById(closeId)?.focus();
  }, [publishing, closeId]);

  const title = impact && phase.name === "ready" ? `Publish version ${impact.next_version} of ${flowName}` : `Publish ${flowName}`;
  const items = impact ? itemsFor(impact.issues) : [];
  const blocked = impact !== null && !impact.valid;
  const reason = blocked ? `Fix ${items.length === 1 ? "the issue" : `the ${items.length} issues`} above first.` : null;
  const audience = impact?.audience ?? null;
  const segmentName = audience ? names.segments.find((segment) => segment.id === audience.segment_id)?.name : undefined;

  return (
    <Dialog title={title} onClose={onClose} wide>
      {phase.name === "loading" ? <LoadingState label="Checking what publishing would do" /> : null}

      {phase.name === "failed" ? (
        <div className="state state-error" role="alert">
          <p className="state-title">{phase.error.title}</p>
          {phase.error.detail ? <p className="state-body">{phase.error.detail}</p> : null}
          <div className="state-actions">
            <Button onClick={() => void load()}>Retry</Button>
          </div>
        </div>
      ) : null}

      {phase.name === "ready" && impact ? (
        <>
          <p className="muted">Nothing below is written until you publish. The numbers come from a dry run.</p>
          <div className="pub-rows">
            <Row label="Checks">
              {items.length === 0 ? (
                <div className="pub-line">
                  <span className="dot dot-quiet" aria-hidden="true" />
                  <span>No issues stop publishing</span>
                </div>
              ) : (
                <ul className="pub-issues" aria-label="Issues that stop publishing">
                  {items.map((item) => {
                    const words = (
                      <>
                        <span className="fl-issue-where">{item.where}</span>
                        <span>{issueText(item.issue, graph)}</span>
                      </>
                    );
                    return (
                      <li key={item.key}>
                        {item.openable ? (
                          <button type="button" className="fl-issue" onClick={() => onPick(item)}>
                            {words}
                          </button>
                        ) : (
                          <div className="fl-issue fl-issue-text">{words}</div>
                        )}
                      </li>
                    );
                  })}
                </ul>
              )}
            </Row>
            <Row label="Who enters now">
              {audience ? (
                <div>
                  About <b>{people(audience.members)}</b>
                  {segmentName ? ` in ${segmentName}` : ""} enter when you publish
                  {audience.computed_at ? `, as counted ${formatAgo(audience.computed_at)}` : ", before the segment has been counted"}.
                  After that, people enter as the trigger fires for them.
                </div>
              ) : (
                <div>
                  No estimate. Only a flow started by a segment, with the people already in it enrolled, has a count of who enters when you
                  publish.
                </div>
              )}
            </Row>
            <Row label="Already in the flow">
              {impact.enrollments_staying_on_older_versions === 0 ? (
                <div>No one is in the flow now, so no one stays on an older version.</div>
              ) : (
                <div>
                  <b>{people(impact.enrollments_staying_on_older_versions)}</b> stay on the version they entered until they finish. Version{" "}
                  {impact.next_version} only applies to people who enter from now on.
                </div>
              )}
            </Row>
            <Row label="Inputs">
              {impact.unhealthy_inputs.length === 0 ? (
                <div>No stale or failing inputs feed this flow.</div>
              ) : (
                <>
                  <ul className="pub-warnings" aria-label="Input warnings">
                    {impact.unhealthy_inputs.map((input) => (
                      <li key={input.source_id}>
                        <span className="dot dot-attention" aria-hidden="true" />
                        <span>
                          <b className="pub-warning">
                            {input.name} is {input.state}.
                          </b>{" "}
                          {input.last_success_at ? `It last synced ${formatAgo(input.last_success_at)}.` : "It has never synced successfully."}
                        </span>
                      </li>
                    ))}
                  </ul>
                  <div>You can still publish. The warning is recorded with the version.</div>
                </>
              )}
            </Row>
          </div>
          {refusal ? (
            <div className="state state-error" role="alert">
              <p className="state-title">{refusal.title}</p>
              {refusal.detail ? <p className="state-body">{refusal.detail}</p> : null}
              <div className="state-actions">
                <Button onClick={() => onPublish()}>Retry</Button>
              </div>
            </div>
          ) : null}
        </>
      ) : null}

      <div className="pub-foot">
        <span id={reasonId} className="faint pub-note">
          {reason ?? "Only admins can publish, pause or archive."}
        </span>
        <span className="visually-hidden pub-announce" role="status">
          {reason}
        </span>
        <Button id={closeId} onClick={onClose}>{publishing ? "Close" : "Cancel"}</Button>
        {phase.name === "ready" && impact ? (
          <Button
            variant="primary"
            disabled={blocked || publishing}
            aria-describedby={blocked ? reasonId : undefined}
            onClick={onPublish}
          >
            {publishing ? "Publishing…" : `Publish version ${impact.next_version}`}
          </Button>
        ) : null}
      </div>
    </Dialog>
  );
}
