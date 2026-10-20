import { useCallback, useEffect, useId, useRef, useState, type ReactNode } from "react";

import { ApiError, client, describeError, unwrap, type components, type ErrorSummary } from "../../api";
import { Button, Dialog, formatAgo } from "../../ui";
import type { IssueItem } from "./Canvas";
import type { Names } from "./describe";
import { issueText } from "./issues";
import type { FlowIssue, Graph } from "./model";

type Impact = components["schemas"]["ImpactBody"];
type Published = components["schemas"]["PublishedBody"];

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
  onPublished: (published: Published) => void;
  /** A publish was refused with these issues, so the canvas can show them. */
  onRefused: (issues: FlowIssue[]) => void;
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
  const { workspaceId, flowId, flowName, graph, names, saveNow, itemsFor, onPick, onPublished, onRefused, onClose } = props;
  const [phase, setPhase] = useState<Phase>({ name: "loading" });
  const [impact, setImpact] = useState<Impact | null>(null);
  const [publishing, setPublishing] = useState(false);
  const [refusal, setRefusal] = useState<ErrorSummary | null>(null);
  const reasonId = useId();
  const live = useRef(true);
  /** The newest call of `saveNow`, kept so Retry does not restart the effect that loads. */
  const save = useRef(saveNow);

  useEffect(() => {
    save.current = saveNow;
  });

  const load = useCallback(async () => {
    setPhase({ name: "loading" });
    setRefusal(null);
    try {
      const saved = await save.current();
      if (!live.current) return;
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
      if (!live.current) return;
      setImpact(body as Impact);
      setPhase({ name: "ready" });
    } catch (error) {
      if (live.current) setPhase({ name: "failed", error: describeError(error) });
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

  async function publish() {
    setPublishing(true);
    setRefusal(null);
    try {
      const published = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/flows/{id}/publish", { params: { path: { ws: workspaceId, id: flowId } } }),
      );
      if (!live.current) return;
      onPublished(published as Published);
    } catch (error) {
      if (!live.current) return;
      setPublishing(false);
      if (error instanceof ApiError && error.code === "flow_invalid") {
        const found = error.flowIssues.map((issue) => issue as FlowIssue);
        setImpact((current) => (current ? { ...current, valid: false, issues: found } : current));
        onRefused(found);
        return;
      }
      setRefusal(describeError(error));
    }
  }

  const title = impact && phase.name === "ready" ? `Publish version ${impact.next_version} of ${flowName}` : `Publish ${flowName}`;
  const items = impact ? itemsFor(impact.issues as FlowIssue[]) : [];
  const blocked = impact !== null && !impact.valid;
  const reason = blocked ? `Fix ${items.length === 1 ? "the issue" : `the ${items.length} issues`} above first.` : null;
  const audience = impact?.audience ?? null;
  const segmentName = audience ? names.segments.find((segment) => segment.id === audience.segment_id)?.name : undefined;

  return (
    <Dialog title={title} onClose={onClose} wide>
      {phase.name === "loading" ? (
        <div className="loading" role="status">
          <span className="spinner" aria-hidden="true" />
          <span>Checking what publishing would do</span>
        </div>
      ) : null}

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
                  {items.map((item) => (
                    <li key={item.key}>
                      {item.openable ? (
                        <button type="button" className="fl-issue" onClick={() => onPick(item)}>
                          <span className="fl-issue-where">{item.where}</span>
                          <span>{issueText(item.issue, graph)}</span>
                        </button>
                      ) : (
                        <div className="fl-issue fl-issue-text">
                          <span className="fl-issue-where">{item.where}</span>
                          <span>{issueText(item.issue, graph)}</span>
                        </div>
                      )}
                    </li>
                  ))}
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
                <Button onClick={() => void publish()}>Retry</Button>
              </div>
            </div>
          ) : null}
        </>
      ) : null}

      <div className="pub-foot">
        <span id={reasonId} className="faint pub-note" role={blocked ? "status" : undefined}>
          {reason ?? "Only admins can publish, pause or archive."}
        </span>
        <Button onClick={onClose}>Cancel</Button>
        {phase.name === "ready" && impact ? (
          <Button
            variant="primary"
            disabled={blocked || publishing}
            aria-describedby={blocked ? reasonId : undefined}
            onClick={() => void publish()}
          >
            {publishing ? "Publishing…" : `Publish version ${impact.next_version}`}
          </Button>
        ) : null}
      </div>
    </Dialog>
  );
}
