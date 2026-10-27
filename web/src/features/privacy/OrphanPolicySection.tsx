import { useState, type SyntheticEvent } from "react";

import { client, describeError, unwrap, useResource, type ErrorSummary } from "../../api";
import { Button, InlineError, Panel, ResourceView, SelectField } from "../../ui";
import { ORPHAN_POLICIES } from "./labels";

export function OrphanPolicySection({ workspaceId }: { workspaceId: string }) {
  const settings = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/privacy/settings", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );

  return (
    <Panel>
      <ResourceView resource={settings} loadingLabel="Loading settings">
        {(data) => <PolicyForm key={data.orphan_policy} workspaceId={workspaceId} saved={data.orphan_policy} onSaved={settings.reload} />}
      </ResourceView>
    </Panel>
  );
}

function PolicyForm({ workspaceId, saved, onSaved }: { workspaceId: string; saved: string; onSaved: () => void }) {
  const [policy, setPolicy] = useState(saved);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const help = ORPHAN_POLICIES.find((option) => option.value === policy)?.help;

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    setNotice(null);
    try {
      await unwrap(
        client.PUT("/api/v1/workspaces/{ws}/privacy/settings", {
          params: { path: { ws: workspaceId } },
          body: { orphan_policy: policy },
        }),
      );
      setNotice("Saved.");
      onSaved();
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
      <p className="muted">What happens when a source retracts a person&apos;s last record and nothing else holds them.</p>
      <SelectField label="Orphan policy" value={policy} onChange={(event) => setPolicy(event.target.value)} hint={help}>
        {ORPHAN_POLICIES.map((option) => (
          <option key={option.value} value={option.value}>
            {option.label}
          </option>
        ))}
      </SelectField>
      {failure ? <InlineError {...failure} /> : null}
      {notice ? (
        <p className="notice" role="status">
          {notice}
        </p>
      ) : null}
      <div>
        <Button type="submit" variant="primary" disabled={busy || policy === saved}>
          {busy ? "Saving" : "Save"}
        </Button>
      </div>
    </form>
  );
}
