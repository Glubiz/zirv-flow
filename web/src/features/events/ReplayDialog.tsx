import { useState } from "react";
import { Link } from "react-router";

import { client, describeError, unwrap, useResource, type components, type ErrorSummary } from "../../api";
import { Button, Dialog, EmptyState, InlineError, LoadingState, Pill, SelectField, shortId, type Tone } from "../../ui";
import { REPLAY_OUTCOMES, reasonLabel, rejectReasonLabel } from "./labels";

type Prediction = components["schemas"]["PredictionBody"];

/** Which rejects a replay covers. */
export type ReplayTarget =
  | { kind: "ids"; ids: string[] }
  | { kind: "filter"; name: string; reason: string };

const TONES: Record<string, Tone> = {
  accept: "neutral",
  flag: "attention",
  still_rejected: "error",
  duplicate: "neutral",
  already_replayed: "neutral",
};

const CAUSES: { value: "definition_change" | "manual"; label: string }[] = [
  { value: "definition_change", label: "I fixed a schema or the settings" },
  { value: "manual", label: "Another reason" },
];

function selection(target: ReplayTarget) {
  return target.kind === "ids"
    ? { ids: target.ids }
    : { filter: { ...(target.name !== "" ? { name: target.name } : {}), ...(target.reason !== "" ? { reason: target.reason } : {}) } };
}

interface ReplayDialogProps {
  workspaceId: string;
  target: ReplayTarget;
  onClose: () => void;
  /** Called after a replay was queued, so the list can refresh. */
  onQueued: () => void;
}

/**
 * Previews what replaying rejects would do, writing nothing, and then queues the replay on
 * confirmation. Downstream triggers stay off unless asked for.
 */
export function ReplayDialog({ workspaceId, target, onClose, onQueued }: ReplayDialogProps) {
  const [cause, setCause] = useState<"definition_change" | "manual">("definition_change");
  const [triggers, setTriggers] = useState(false);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [queued, setQueued] = useState<{ selected: number; traceId: string } | null>(null);

  const preview = useResource(
    () =>
      unwrap(
        client.POST("/api/v1/workspaces/{ws}/ingest-rejects/replay", {
          params: { path: { ws: workspaceId } },
          body: { ...selection(target), cause: "manual", dry_run: true },
        }),
      ),
    [workspaceId, JSON.stringify(target)],
  );

  async function confirm() {
    setBusy(true);
    setFailure(null);
    try {
      const reply = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/ingest-rejects/replay", {
          params: { path: { ws: workspaceId } },
          body: { ...selection(target), cause, emit_triggers: triggers, dry_run: false },
        }),
      );
      setQueued({ selected: reply.selected, traceId: reply.trace_id ?? "" });
      onQueued();
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  if (queued !== null) {
    return (
      <Dialog title="Replay queued" onClose={onClose}>
        <div className="stack">
          <p role="status">
            {queued.selected} {queued.selected === 1 ? "reject is" : "rejects are"} being replayed. Events that now pass are stored once, flagged{" "}
            <span className="mono">replayed</span>
            {triggers ? "." : <> and <span className="mono">no_triggers</span>.</>}
          </p>
          {queued.traceId !== "" ? (
            <p>
              <Link to={`/w/${workspaceId}/traces/${queued.traceId}`}>Follow the decisions on the trace</Link>
            </p>
          ) : null}
          <div className="dialog-actions">
            <Button variant="primary" onClick={onClose}>
              Done
            </Button>
          </div>
        </div>
      </Dialog>
    );
  }

  const stored = preview.status === "ready" ? preview.data.totals.accept + preview.data.totals.flag : 0;

  return (
    <Dialog title="Replay rejected events" onClose={onClose} wide>
      <div className="stack">
        <p className="muted">
          Each reject is checked again against the schemas and settings in force now. This preview writes nothing. Only the stored, redacted
          payload is used, and an event that is already stored under the same message id is not stored twice.
        </p>
        {preview.status === "loading" ? <LoadingState label="Checking the rejects" /> : null}
        {preview.status === "error" ? (
          <InlineError {...describeError(preview.error)} />
        ) : null}
        {preview.status === "ready" ? <Preview rejects={preview.data.rejects} totals={preview.data.totals} selected={preview.data.selected} truncated={preview.data.truncated} /> : null}
        <SelectField label="Why are you replaying?" value={cause} onChange={(event) => setCause(event.target.value === "manual" ? "manual" : "definition_change")}>
          {CAUSES.map((option) => (
            <option key={option.value} value={option.value}>
              {option.label}
            </option>
          ))}
        </SelectField>
        <label className="check">
          <input type="checkbox" checked={triggers} onChange={(event) => setTriggers(event.target.checked)} />
          <span>
            Emit downstream triggers
            <span className="field-hint">
              {" "}
              Off by default. Replayed events are flagged <span className="mono">no_triggers</span>, so flows that start on events ignore them. Turn it on
              only if people should still be sent what these events would have started.
            </span>
          </span>
        </label>
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={() => void confirm()} disabled={busy || stored === 0}>
            {busy ? "Queuing" : stored === 0 ? "Nothing to replay" : `Replay ${stored} ${stored === 1 ? "event" : "events"}`}
          </Button>
        </div>
      </div>
    </Dialog>
  );
}

function Preview({
  rejects,
  totals,
  selected,
  truncated,
}: {
  rejects: Prediction[];
  totals: components["schemas"]["TotalsBody"];
  selected: number;
  truncated: boolean;
}) {
  if (selected === 0) {
    return <EmptyState title="No rejects to replay">Nothing open matches. Rejects that were replayed already are left out.</EmptyState>;
  }
  return (
    <>
      <p role="status">
        {selected} {selected === 1 ? "reject" : "rejects"}: {totals.accept} will be stored, {totals.flag} stored and flagged, {totals.still_rejected} still rejected,{" "}
        {totals.duplicate} already stored, {totals.already_replayed} replayed before.
      </p>
      {truncated ? (
        <p className="muted">
          The table lists the first {rejects.length} of {selected} rejects. The totals above cover all of them.
        </p>
      ) : null}
      <div className="table-scroll">
        <table className="table">
          <caption>Predicted outcome of each reject</caption>
          <thead>
            <tr>
              <th scope="col">Reject</th>
              <th scope="col">Outcome</th>
              <th scope="col">Why</th>
            </tr>
          </thead>
          <tbody>
            {rejects.map((reject) => (
              <tr key={reject.reject_id}>
                <td className="mono">{shortId(reject.reject_id)}</td>
                <td>
                  <Pill tone={TONES[reject.outcome] ?? "neutral"}>{REPLAY_OUTCOMES[reject.outcome] ?? reject.outcome}</Pill>
                </td>
                <td>
                  {reject.reason ? <p>{rejectReasonLabel(reject.reason)}</p> : null}
                  {reject.reasons.length > 0 ? (
                    <ul className="records list-plain">
                      {reject.reasons.map((reason, index) => (
                        <li key={index}>
                          <span className="mono">{reason.property}</span> <span className="faint">{reasonLabel(reason.reason)}</span>
                        </li>
                      ))}
                    </ul>
                  ) : null}
                  {reject.redacted_type_changed.length > 0 ? (
                    <p className="faint">
                      Redaction changed the type of <span className="mono">{reject.redacted_type_changed.join(", ")}</span>, so the stored value cannot
                      pass a schema that expects another type.
                    </p>
                  ) : null}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </>
  );
}
