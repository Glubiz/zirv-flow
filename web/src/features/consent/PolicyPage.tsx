import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { useParams } from "react-router";

import { client, describeError, unwrap, useResource } from "../../api";
import { Button, Dialog, ErrorState, InlineError, LoadingState, PageHeader, Pill } from "../../ui";
import { useWorkspaceAccess } from "../members";
import { describeCondition, useLeaveGuard, type ConditionNode } from "../segments";
import { ConsentNav } from "./ConsentNav";
import type { Dimension } from "./DimensionDialog";
import { EvaluatePanel, type Evaluation } from "./EvaluatePanel";
import { PlusIcon } from "./policyIcons";
import { MAX_RULES, ruleIssueMessage } from "./policyModel";
import { PolicyVersions } from "./PolicyVersions";
import { RuleCard } from "./RuleCard";
import { usePolicyDraft } from "./usePolicyDraft";

export function PolicyPage() {
  const { ws = "" } = useParams();
  return <PolicyView key={ws} workspaceId={ws} />;
}

type View = "rules" | "versions";

const VIEWS: readonly { id: View; label: string }[] = [
  { id: "rules", label: "Rules" },
  { id: "versions", label: "Version history" },
];

type Names = { lists: { id: string; name: string }[]; segments: { id: string; name: string }[] };

/** A condition in one line, groups included, for the closed card. */
function summaryOf(node: ConditionNode, names: Names): string {
  if ("all" in node) {
    return node.all.length === 0 ? "Everyone" : node.all.map((child) => summaryOf(child, names)).join(" and ");
  }
  if ("any" in node) {
    const parts = node.any.map((child) => summaryOf(child, names));
    return parts.length === 0 ? "No one" : parts.length === 1 ? (parts[0] ?? "") : `(${parts.join(" or ")})`;
  }
  if ("not" in node) return `not (${summaryOf(node.not, names)})`;
  return describeCondition(node, names).replace(/^./, (first) => first.toLowerCase());
}

function PolicyView({ workspaceId }: { workspaceId: string }) {
  const access = useWorkspaceAccess(workspaceId);
  const canManage = access.status === "ready" && access.data.canManage;
  const canWrite = access.status === "ready" && (access.data.canManage || access.data.role !== null);
  const draft = usePolicyDraft(workspaceId);
  const guard = useLeaveGuard(draft.dirty);
  const dimensions = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/consent/dimensions", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const lists = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/lists", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const segments = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/segments", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const names: Names = {
    lists: lists.status === "ready" ? lists.data : [],
    segments: segments.status === "ready" ? segments.data : [],
  };
  const dimensionList: readonly Dimension[] = dimensions.status === "ready" ? dimensions.data : [];

  const [view, setView] = useState<View>("rules");
  const [open, setOpen] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);
  const [publishing, setPublishing] = useState(false);
  const [announcement, setAnnouncement] = useState("");
  const [notice, setNotice] = useState<string | null>(null);
  const [decided, setDecided] = useState<Evaluation | null>(null);
  const focusAfter = useRef<{ key: string; by: -1 | 1 } | null>(null);
  const list = useRef<HTMLOListElement>(null);

  // After a rule moves, the focus stays on the button that moved it, or on its partner when that one cannot go further.
  useEffect(() => {
    const wanted = focusAfter.current;
    if (wanted === null) return;
    focusAfter.current = null;
    const card = Array.from(list.current?.querySelectorAll<HTMLElement>("[data-rule]") ?? []).find((element) => element.dataset["rule"] === wanted.key);
    const same = card?.querySelector<HTMLButtonElement>(`[data-move="${wanted.by === -1 ? "up" : "down"}"]`);
    const other = card?.querySelector<HTMLButtonElement>(`[data-move="${wanted.by === -1 ? "down" : "up"}"]`);
    (same && !same.disabled ? same : other)?.focus();
  });

  if (draft.load.status === "loading") {
    return (
      <div className="page">
        <ConsentNav workspaceId={workspaceId} />
        <LoadingState label="Loading the consent policy" />
      </div>
    );
  }
  if (draft.load.status === "error") {
    return (
      <div className="page">
        <ConsentNav workspaceId={workspaceId} />
        <ErrorState {...describeError(draft.load.error)} onRetry={draft.reload} />
      </div>
    );
  }

  const live = draft.publishedVersion;
  const status = draft.dirty ? "Unsaved changes" : live === null ? "Draft · nothing published" : `Draft saved · v${live} live`;

  function move(key: string, by: -1 | 1) {
    const at = draft.rules.findIndex((rule) => rule.key === key);
    const rule = draft.rules[at];
    if (rule === undefined || at + by < 0 || at + by >= draft.rules.length) return;
    focusAfter.current = { key, by };
    draft.move(key, by);
    setAnnouncement(`Moved ${rule.name} ${by === -1 ? "up" : "down"} to position ${at + by + 1} of ${draft.rules.length}.`);
  }

  function addRule() {
    const rule = draft.add();
    setOpen(rule.key);
    setAnnouncement(`Added a rule at position ${draft.rules.length + 1}.`);
  }

  async function save() {
    setNotice(null);
    if (await draft.save()) setNotice("Draft saved.");
  }

  async function confirmPublish() {
    const version = await draft.publish();
    setPublishing(false);
    if (version !== null) setNotice(`Published as version ${version}. Sends that start from now on use it.`);
  }

  function onTabKey(event: KeyboardEvent<HTMLButtonElement>) {
    const step = event.key === "ArrowRight" ? 1 : event.key === "ArrowLeft" ? -1 : 0;
    if (step === 0) return;
    event.preventDefault();
    const tabs = VIEWS.filter((entry) => entry.id === "rules" || canManage);
    const next = tabs[(tabs.findIndex((entry) => entry.id === view) + step + tabs.length) % tabs.length];
    if (!next) return;
    setView(next.id);
    document.getElementById(`policy-tab-${next.id}`)?.focus();
  }

  const deletingRule = draft.rules.find((rule) => rule.key === deleting);
  const decidingId = decided?.rule?.id ?? null;

  return (
    <div className="page">
      <ConsentNav workspaceId={workspaceId} />
      <PageHeader
        title="Consent policy"
        lede="The rules that can stop a send even when a person has consented. Edit the draft, save it, and publish it when it is ready."
      />
      <div className="policy-bar">
        <Pill tone={draft.dirty ? "attention" : "neutral"}>{status}</Pill>
        {canManage ? (
          <div role="tablist" aria-label="Policy views" className="seg-tabs">
            {VIEWS.map((entry) => (
              <button
                key={entry.id}
                type="button"
                role="tab"
                id={`policy-tab-${entry.id}`}
                aria-selected={view === entry.id}
                aria-controls={`policy-panel-${entry.id}`}
                tabIndex={view === entry.id ? 0 : -1}
                className={view === entry.id ? "seg seg-on" : "seg"}
                onClick={() => setView(entry.id)}
                onKeyDown={onTabKey}
              >
                {entry.label}
              </button>
            ))}
          </div>
        ) : null}
        <div className="row policy-bar-actions">
          {canWrite ? (
            <Button onClick={() => void save()} disabled={!draft.dirty || draft.busy !== null}>
              {draft.busy === "saving" ? "Saving" : "Save draft"}
            </Button>
          ) : null}
          {canManage ? (
            <Button variant="primary" onClick={() => setPublishing(true)} disabled={draft.dirty || draft.busy !== null}>
              Publish…
            </Button>
          ) : null}
        </div>
      </div>
      {canManage && draft.dirty ? <p className="faint">Save the draft to publish it.</p> : null}
      {!canWrite ? <p className="muted">You can read the policy. Members of the workspace can edit the draft, and admins publish it.</p> : null}
      <p className="visually-hidden" role="status" aria-live="polite">
        {announcement}
      </p>
      {notice ? (
        <p className="notice" role="status">
          {notice}
        </p>
      ) : null}
      {draft.failure ? <InlineError {...draft.failure} /> : null}
      {draft.stale ? (
        <div className="notice" role="alert">
          <p>The draft was saved by someone else after you opened this page. Your edits were not saved.</p>
          <Button onClick={draft.discard}>Load the latest draft</Button>
        </div>
      ) : null}
      {draft.issues.general.map((code) => (
        <p className="field-error" role="alert" key={code}>
          {ruleIssueMessage(code)}
        </p>
      ))}

      {view === "versions" && canManage ? (
        <div role="tabpanel" id="policy-panel-versions" aria-labelledby="policy-tab-versions">
          <PolicyVersions workspaceId={workspaceId} published={live} />
        </div>
      ) : (
        <div className="policy-layout" role="tabpanel" id="policy-panel-rules" aria-labelledby={canManage ? "policy-tab-rules" : undefined}>
          <section aria-labelledby="rules-h" className="policy-main">
            <div className="policy-intro">
              <div>
                <h2 id="rules-h" className="panel-title">
                  Rules, checked top to bottom
                </h2>
                <p className="muted">
                  Rules can only stop a send. The first rule that matches skips it as <span className="mono">no_consent</span> and names the rule.
                  Transactional templates skip the policy; suppressions still apply to them.
                </p>
              </div>
              {canWrite ? (
                <Button onClick={addRule} disabled={draft.rules.length >= MAX_RULES}>
                  <PlusIcon size={16} /> Add a rule
                </Button>
              ) : null}
            </div>
            <ol className="rules" ref={list} aria-label="Rules in the order they are checked">
              {draft.rules.map((rule, at) => (
                <RuleCard
                  key={rule.key}
                  workspaceId={workspaceId}
                  rule={rule}
                  index={at}
                  total={draft.rules.length}
                  open={open === rule.key || draft.issues.rule.has(at) || draft.issues.condition.has(at)}
                  readOnly={!canWrite}
                  summary={rule.condition === null ? "" : summaryOf(rule.condition, names)}
                  dimensions={dimensionList}
                  ruleIssues={draft.issues.rule.get(at) ?? []}
                  conditionIssues={draft.issues.condition.get(at) ?? []}
                  decides={decidingId !== null && rule.id === decidingId}
                  onToggle={() => setOpen(open === rule.key ? null : rule.key)}
                  onMove={(by) => move(rule.key, by)}
                  onRename={(name) => draft.change(rule.key, { name })}
                  onCondition={(condition) => draft.change(rule.key, { condition })}
                  onDelete={() => setDeleting(rule.key)}
                />
              ))}
            </ol>
            <div className="policy-end">
              <span className="st st-ok">No rule matched: send</span>
              <span>The send goes on to the suppression check.</span>
            </div>
          </section>
          {canManage ? <EvaluatePanel workspaceId={workspaceId} dimensions={dimensionList} onResult={setDecided} /> : null}
        </div>
      )}

      {deletingRule ? (
        <Dialog title={`Delete ${deletingRule.name}?`} onClose={() => setDeleting(null)}>
          <p className="muted">The rule leaves the draft. The published policy keeps it until you publish again.</p>
          <div className="dialog-actions">
            <Button onClick={() => setDeleting(null)}>Keep rule</Button>
            <Button
              variant="primary"
              onClick={() => {
                draft.remove(deletingRule.key);
                setAnnouncement(`Deleted ${deletingRule.name}.`);
                setDeleting(null);
              }}
            >
              Delete rule
            </Button>
          </div>
        </Dialog>
      ) : null}
      {publishing ? (
        <Dialog title="Publish the policy?" onClose={() => setPublishing(false)}>
          <p className="muted">
            The {draft.rules.length} {draft.rules.length === 1 ? "rule" : "rules"} of this draft become version {(live ?? 0) + 1}. Sends whose consent check
            starts after you publish use it; a send already past that check finishes under the version it read.
          </p>
          <div className="dialog-actions">
            <Button onClick={() => setPublishing(false)}>Cancel</Button>
            <Button variant="primary" onClick={() => void confirmPublish()} disabled={draft.busy !== null}>
              {draft.busy === "publishing" ? "Publishing" : "Publish"}
            </Button>
          </div>
        </Dialog>
      ) : null}
      {guard.pending !== null ? (
        <Dialog title="Leave without saving?" onClose={guard.stay}>
          <p className="muted">You have changes to the draft that are not saved. If you leave now, they are lost.</p>
          <div className="dialog-actions">
            <Button onClick={guard.stay}>Keep editing</Button>
            <Button variant="primary" onClick={guard.leave}>
              Leave without saving
            </Button>
          </div>
        </Dialog>
      ) : null}
    </div>
  );
}
