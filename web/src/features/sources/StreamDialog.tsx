import { useState, type SyntheticEvent } from "react";

import { client, describeError, unwrap, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError, SelectField, TextField } from "../../ui";
import { FreshnessBadge } from "./FreshnessBadge";
import { lastSuccessLabel, syncModeLabel, thresholdLabel, type Stream } from "./labels";

interface StreamDialogProps {
  workspaceId: string;
  sourceId: string;
  stream: Stream;
  onClose: () => void;
  onSaved: (stream: Stream) => void;
}

/** Whether a stream is read, and how: the sync mode, cursor field, primary key and staleness. */
export function StreamDialog({ workspaceId, sourceId, stream, onClose, onSaved }: StreamDialogProps) {
  const modes = stream.discovered.sync_modes;
  const fields = stream.discovered.fields;
  const [enabled, setEnabled] = useState(stream.enabled);
  const [syncMode, setSyncMode] = useState(stream.sync_mode);
  const [cursor, setCursor] = useState(stream.cursor_field ?? stream.discovered.default_cursor_field ?? "");
  const [primaryKey, setPrimaryKey] = useState<string[]>(
    stream.primary_key.length > 0 ? stream.primary_key : stream.discovered.default_primary_key,
  );
  const [staleness, setStaleness] = useState(
    stream.staleness_threshold_seconds ? String(stream.staleness_threshold_seconds) : "",
  );
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  const needsCursor = syncMode === "incremental" && cursor === "";
  const stalenessValue = staleness.trim() === "" ? null : Number(staleness);
  const stalenessOk = stalenessValue === null || (Number.isInteger(stalenessValue) && stalenessValue > 0);

  function togglePrimaryKey(name: string, on: boolean) {
    setPrimaryKey((current) => (on ? [...current, name] : current.filter((entry) => entry !== name)));
  }

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      const saved = await unwrap(
        client.PUT("/api/v1/workspaces/{ws}/sources/{id}/streams/{name}", {
          params: { path: { ws: workspaceId, id: sourceId, name: stream.name } },
          body: {
            enabled,
            sync_mode: syncMode,
            cursor_field: cursor === "" ? null : cursor,
            primary_key: primaryKey,
            staleness_threshold_seconds: stalenessValue,
          },
        }),
      );
      onSaved(saved);
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title={`Set up ${stream.name}`} onClose={onClose}>
      {stream.freshness ? (
        <dl className="facts">
          <div className="fact">
            <dt>Freshness</dt>
            <dd>
              <FreshnessBadge state={stream.freshness.state} />
            </dd>
          </div>
          <div className="fact">
            <dt>Last succeeded</dt>
            <dd>{lastSuccessLabel(stream.freshness)}</dd>
          </div>
          <div className="fact">
            <dt>Stale after</dt>
            <dd>{thresholdLabel(stream.freshness)}</dd>
          </div>
        </dl>
      ) : null}
      <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
        <label className="check">
          <input type="checkbox" checked={enabled} onChange={(event) => setEnabled(event.target.checked)} />
          <span>Read this stream</span>
        </label>
        <SelectField label="Sync mode" value={syncMode} onChange={(event) => setSyncMode(event.target.value)}>
          {modes.map((mode) => (
            <option key={mode} value={mode}>
              {syncModeLabel(mode)}
            </option>
          ))}
        </SelectField>
        <SelectField
          label="Cursor field"
          value={cursor}
          onChange={(event) => setCursor(event.target.value)}
          hint="The field that orders records by when they changed. An incremental stream needs one."
          error={needsCursor ? "Choose a cursor field for an incremental stream." : undefined}
        >
          <option value="">None</option>
          {fields.map((field) => (
            <option key={field.name} value={field.name}>
              {field.name}
            </option>
          ))}
        </SelectField>
        <fieldset className="checks">
          <legend>Primary key</legend>
          {fields.map((field) => (
            <label className="check" key={field.name}>
              <input
                type="checkbox"
                checked={primaryKey.includes(field.name)}
                onChange={(event) => togglePrimaryKey(field.name, event.target.checked)}
              />
              <span>{field.name}</span>
            </label>
          ))}
        </fieldset>
        <TextField
          label="Staleness threshold (seconds)"
          hint="Optional. Seconds without a successful run before the stream counts as stale. Left empty, it is twice the source schedule's interval, or never when the source has no schedule."
          type="number"
          min={1}
          value={staleness}
          onChange={(event) => setStaleness(event.target.value)}
          error={stalenessOk ? undefined : "Use a whole number above zero."}
        />
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button type="submit" variant="primary" disabled={busy || needsCursor || !stalenessOk}>
            {busy ? "Saving" : "Save stream"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
