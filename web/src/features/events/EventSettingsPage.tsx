import { useState, type SyntheticEvent } from "react";
import { useParams } from "react-router";

import { client, describeError, unwrap, useResource, type components, type ErrorSummary } from "../../api";
import { Button, InlineError, PageHeader, Panel, ResourceView, SelectField, TextField } from "../../ui";
import { useWorkspaceAccess } from "../members";
import { EventsNav } from "./EventsNav";
import { UNPLANNED, splitList } from "./labels";

type Policy = components["schemas"]["EventPolicyBody"];

export function EventSettingsPage() {
  const { ws = "" } = useParams();
  const access = useWorkspaceAccess(ws);
  const canManage = access.status === "ready" && access.data.canManage;

  return (
    <div className="page">
      <PageHeader
        title="Event settings"
        lede="How the workspace treats events without a schema, events with odd timestamps, and personal data in event properties."
      />
      <EventsNav workspaceId={ws} />
      <PolicyView key={ws} workspaceId={ws} canManage={canManage} />
    </div>
  );
}

function PolicyView({ workspaceId, canManage }: { workspaceId: string; canManage: boolean }) {
  const policy = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/event-policy", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );

  return (
    <ResourceView resource={policy} loadingLabel="Loading settings">
      {(data) => (
        <PolicyForm key={JSON.stringify(data)} workspaceId={workspaceId} policy={data} canManage={canManage} onSaved={policy.reload} />
      )}
    </ResourceView>
  );
}

interface PolicyFormProps {
  workspaceId: string;
  policy: Policy;
  canManage: boolean;
  onSaved: () => void;
}

function PolicyForm({ workspaceId, policy, canManage, onSaved }: PolicyFormProps) {
  const [unplanned, setUnplanned] = useState(policy.unplanned);
  const [skew, setSkew] = useState(String(policy.future_skew_seconds));
  const [lateness, setLateness] = useState(String(policy.lateness_seconds));
  const [drop, setDrop] = useState((policy.redaction.drop ?? []).join(", "));
  const [hash, setHash] = useState((policy.redaction.hash ?? []).join(", "));
  const [truncate, setTruncate] = useState((policy.redaction.truncate_ip ?? []).join(", "));
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  const next: Policy = {
    unplanned,
    future_skew_seconds: Number(skew),
    lateness_seconds: Number(lateness),
    redaction: { drop: splitList(drop), hash: splitList(hash), truncate_ip: splitList(truncate) },
  };
  const unchanged = JSON.stringify(next) === JSON.stringify(policy);
  const invalid = !Number.isInteger(next.future_skew_seconds) || !Number.isInteger(next.lateness_seconds) || skew.trim() === "" || lateness.trim() === "";

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      await unwrap(client.PUT("/api/v1/workspaces/{ws}/event-policy", { params: { path: { ws: workspaceId } }, body: next }));
      onSaved();
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  const readOnly = !canManage;
  return (
    <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
      {readOnly ? <p className="muted">Only workspace admins and the instance owner can change these settings.</p> : null}
      <Panel title="Events without a schema">
        <SelectField
          label="When an event has no published schema"
          hint={UNPLANNED.find((option) => option.value === unplanned)?.help}
          value={unplanned}
          onChange={(event) => setUnplanned(event.target.value)}
          disabled={readOnly}
        >
          {UNPLANNED.map((option) => (
            <option key={option.value} value={option.value}>
              {option.label}
            </option>
          ))}
        </SelectField>
      </Panel>
      <Panel title="Timestamps">
        <div className="stack">
          <TextField
            label="Future skew in seconds"
            type="number"
            inputMode="numeric"
            hint="A timestamp further ahead than this is replaced by the receipt time and flagged. 0 to 86400."
            value={skew}
            onChange={(event) => setSkew(event.target.value)}
            disabled={readOnly}
          />
          <TextField
            label="Lateness window in seconds"
            type="number"
            inputMode="numeric"
            hint="An event older than this is accepted and flagged late. 0 to 31536000."
            value={lateness}
            onChange={(event) => setLateness(event.target.value)}
            disabled={readOnly}
          />
        </div>
      </Panel>
      <Panel title="Redaction">
        <div className="stack">
          <p className="muted">
            Applied before an event or a reject is stored. Use dot-separated paths such as <span className="mono">context.ip</span>,
            separated by commas.
          </p>
          <TextField
            label="Remove"
            mono
            hint="The value is dropped."
            value={drop}
            onChange={(event) => setDrop(event.target.value)}
            disabled={readOnly}
          />
          <TextField
            label="Replace with a keyed hash"
            mono
            hint="Equal values stay equal inside this workspace, but cannot be read back."
            value={hash}
            onChange={(event) => setHash(event.target.value)}
            disabled={readOnly}
          />
          <TextField
            label="Truncate IP addresses"
            mono
            hint="The last IPv4 octet is zeroed; an IPv6 address keeps its first 48 bits. Anything else is removed."
            value={truncate}
            onChange={(event) => setTruncate(event.target.value)}
            disabled={readOnly}
          />
        </div>
      </Panel>
      {failure ? <InlineError {...failure} /> : null}
      {canManage ? (
        <div>
          <Button type="submit" variant="primary" disabled={busy || unchanged || invalid}>
            {busy ? "Saving" : "Save settings"}
          </Button>
        </div>
      ) : null}
    </form>
  );
}
