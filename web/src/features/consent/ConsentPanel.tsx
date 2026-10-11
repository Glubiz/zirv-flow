import { useState, type SyntheticEvent } from "react";

import { client, describeError, unwrap, useResource, type ErrorSummary } from "../../api";
import { Button, EmptyState, formatDateTime, InlineError, Panel, Pill, ResourceView, SelectField } from "../../ui";
import { statusLabel, STATUSES } from "./labels";

interface ConsentPanelProps {
  workspaceId: string;
  personId: string;
  /** Record a choice for the person: any member, admin or the owner. */
  canWrite: boolean;
}

const CHANNEL_WIDE = "";

/** A person's consent for email, channel-wide and per topic, with a form to record a choice. */
export function ConsentPanel({ workspaceId, personId, canWrite }: ConsentPanelProps) {
  const consent = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/people/{id}/consent", {
          params: { path: { ws: workspaceId, id: personId } },
        }),
      ),
    [workspaceId, personId],
  );

  return (
    <Panel title="Consent">
      <ResourceView
        resource={consent}
        loadingLabel="Loading consent"
        isEmpty={(data) => data.states.length === 0}
        empty={
          <EmptyState title="No choice on record">
            Until this person subscribes or unsubscribes, their consent counts as unknown.
          </EmptyState>
        }
      >
        {(data) => (
          <ul className="list-plain stack-tight">
            {data.states.map((state) => (
              <li className="row" key={state.topic_key ?? CHANNEL_WIDE}>
                <span className="strong">{state.topic_name ?? "All email"}</span>
                <Pill tone={state.status === "subscribed" ? "neutral" : "attention"}>{statusLabel(state.status)}</Pill>
                <span className="faint">
                  {state.explicit ? "Chosen by the person" : "Default"} · {formatDateTime(state.changed_at)}
                </span>
              </li>
            ))}
          </ul>
        )}
      </ResourceView>
      {canWrite ? <RecordChoice workspaceId={workspaceId} personId={personId} onRecorded={consent.reload} /> : null}
    </Panel>
  );
}

interface RecordChoiceProps {
  workspaceId: string;
  personId: string;
  onRecorded: () => void;
}

function RecordChoice({ workspaceId, personId, onRecorded }: RecordChoiceProps) {
  const topics = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/topics", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const [topic, setTopic] = useState(CHANNEL_WIDE);
  const [status, setStatus] = useState("unsubscribed");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    setNotice(null);
    try {
      await unwrap(
        client.PUT("/api/v1/workspaces/{ws}/people/{id}/consent", {
          params: { path: { ws: workspaceId, id: personId } },
          body: { channel: "email", status, topic_key: topic === CHANNEL_WIDE ? null : topic },
        }),
      );
      setNotice(`Recorded: ${statusLabel(status).toLowerCase()}.`);
      onRecorded();
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  const choices = topics.status === "ready" ? topics.data.filter((candidate) => !candidate.archived) : [];

  return (
    <form className="stack" onSubmit={(event) => void submit(event)} aria-label="Record a consent choice">
      <div className="row">
        <SelectField label="Applies to" value={topic} onChange={(event) => setTopic(event.target.value)}>
          <option value={CHANNEL_WIDE}>All email</option>
          {choices.map((candidate) => (
            <option key={candidate.id} value={candidate.key}>
              {candidate.name}
            </option>
          ))}
        </SelectField>
        <SelectField label="Choice" value={status} onChange={(event) => setStatus(event.target.value)}>
          {STATUSES.map((option) => (
            <option key={option.value} value={option.value}>
              {option.label}
            </option>
          ))}
        </SelectField>
      </div>
      {failure ? <InlineError {...failure} /> : null}
      {notice ? (
        <p className="notice" role="status">
          {notice}
        </p>
      ) : null}
      <div>
        <Button type="submit" variant="primary" disabled={busy}>
          {busy ? "Recording" : "Record choice"}
        </Button>
      </div>
    </form>
  );
}
