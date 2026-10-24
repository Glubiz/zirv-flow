import { useState, type SyntheticEvent } from "react";
import { useParams } from "react-router";

import { client, describeError, unwrap, useResource, type ErrorSummary } from "../../api";
import { Button, ErrorState, InlineError, LoadingState, PageHeader, Panel, SelectField, TextField } from "../../ui";
import { useWorkspaceAccess } from "../members";
import {
  check,
  draftFromApi,
  draftToApi,
  hasProblems,
  MAX_CAPS,
  nextCapKey,
  sameDraft,
  WEEKDAYS,
  windowText,
  type CapDraft,
  type SendSettings,
  type SettingsDraft,
} from "./model";

export function SendingSettingsPage() {
  const { ws = "" } = useParams();
  const access = useWorkspaceAccess(ws);
  const canManage = access.status === "ready" && access.data.canManage;
  const settings = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/sending/settings", { params: { path: { ws } } })),
    [ws],
  );
  const workspace = useResource(() => unwrap(client.GET("/api/v1/workspaces/{ws}", { params: { path: { ws } } })), [ws]);

  return (
    <div className="page">
      <PageHeader
        title="Sending limits"
        lede="How often one person can hear from you, and the hours when nothing is sent. Both apply to marketing messages; transactional messages skip them."
      />
      {settings.status === "loading" ? <LoadingState label="Loading the sending limits" /> : null}
      {settings.status === "error" ? <ErrorState {...describeError(settings.error)} onRetry={settings.reload} /> : null}
      {settings.status === "ready" ? (
        <SettingsForm
          key={`${ws}:${JSON.stringify(settings.data)}`}
          workspaceId={ws}
          stored={settings.data}
          zone={workspace.status === "ready" ? workspace.data.timezone : null}
          canManage={canManage}
          onSaved={settings.reload}
        />
      ) : null}
    </div>
  );
}

interface FormProps {
  workspaceId: string;
  stored: SendSettings;
  zone: string | null;
  canManage: boolean;
  onSaved: () => void;
}

function SettingsForm({ workspaceId, stored, zone, canManage, onSaved }: FormProps) {
  const [original] = useState<SettingsDraft>(() => draftFromApi(stored));
  const [draft, setDraft] = useState<SettingsDraft>(original);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [tried, setTried] = useState(false);
  const problems = check(draft);
  const dirty = !sameDraft(draft, original);
  const readOnly = !canManage;

  function setCap(key: number, change: Partial<CapDraft>) {
    setDraft((current) => ({ ...current, caps: current.caps.map((cap) => (cap.key === key ? { ...cap, ...change } : cap)) }));
  }

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setTried(true);
    setNotice(null);
    if (hasProblems(problems)) return;
    setBusy(true);
    setFailure(null);
    try {
      await unwrap(
        client.PUT("/api/v1/workspaces/{ws}/sending/settings", {
          params: { path: { ws: workspaceId } },
          body: draftToApi(draft),
        }),
      );
      setNotice("Saved. The limits apply to messages that reach the check from now on.");
      onSaved();
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  const quiet = draft.quiet;

  return (
    <form className="stack" onSubmit={(event) => void submit(event)} noValidate aria-label="Sending limits">
      <Panel title="Frequency caps">
        <div className="stack">
          <p className="muted">
            A cap allows at most a number of messages to one person on a channel in any window of that length. When a message would go over,
            either skip it or hold it until a slot frees up. A person has to pass every cap.
          </p>
          {draft.caps.length === 0 ? (
            <p className="faint">No caps. A person can get as many messages as your flows send.</p>
          ) : (
            <ul className="sending-caps" aria-label="Frequency caps">
              {draft.caps.map((cap, at) => {
                const found = tried ? problems.caps.get(cap.key) : undefined;
                const label = `cap ${at + 1}`;
                return (
                  <li key={cap.key} className="sending-cap" role="group" aria-label={`Cap ${at + 1}`}>
                    <SelectField label={`Channel of ${label}`} value={cap.channel} onChange={(event) => setCap(cap.key, { channel: event.target.value })} disabled={readOnly}>
                      <option value="email">Email</option>
                    </SelectField>
                    <TextField
                      label={`Window of ${label} in hours`}
                      hint={Number.isInteger(Number(cap.hours)) && Number(cap.hours) > 0 ? `Any ${windowText(Number(cap.hours))}` : "Up to 744 hours"}
                      type="number"
                      inputMode="numeric"
                      min={1}
                      max={744}
                      value={cap.hours}
                      onChange={(event) => setCap(cap.key, { hours: event.target.value })}
                      disabled={readOnly}
                      error={found?.hours}
                    />
                    <TextField
                      label={`Limit of ${label}`}
                      hint="Messages per person"
                      type="number"
                      inputMode="numeric"
                      min={1}
                      value={cap.limit}
                      onChange={(event) => setCap(cap.key, { limit: event.target.value })}
                      disabled={readOnly}
                      error={found?.limit}
                    />
                    <SelectField
                      label={`Over the limit for ${label}`}
                      value={cap.mode}
                      onChange={(event) => setCap(cap.key, { mode: event.target.value === "defer" ? "defer" : "skip" })}
                      disabled={readOnly}
                    >
                      <option value="skip">Skip the message</option>
                      <option value="defer">Hold it until a slot frees up</option>
                    </SelectField>
                    {readOnly ? null : (
                      <Button onClick={() => setDraft((current) => ({ ...current, caps: current.caps.filter((entry) => entry.key !== cap.key) }))}>
                        Remove <span className="visually-hidden">{label}</span>
                      </Button>
                    )}
                  </li>
                );
              })}
            </ul>
          )}
          {readOnly ? null : (
            <div>
              <Button
                onClick={() =>
                  setDraft((current) => ({
                    ...current,
                    caps: [...current.caps, { key: nextCapKey(), channel: "email", hours: "24", limit: "3", mode: "skip" }],
                  }))
                }
                disabled={draft.caps.length >= MAX_CAPS}
              >
                Add a cap
              </Button>
            </div>
          )}
        </div>
      </Panel>

      <Panel title="Quiet hours">
        <div className="stack">
          <p className="muted">
            Nothing is sent inside the quiet period. A message that reaches the check then is held until the period ends. A period belongs to
            the day it starts on, so a night from Friday 21:00 to Saturday 08:00 is a Friday period.
          </p>
          <label className="check">
            <input
              type="checkbox"
              checked={quiet !== null}
              disabled={readOnly}
              onChange={(event) =>
                setDraft((current) => ({
                  ...current,
                  quiet: event.target.checked ? { start: "21:00", end: "08:00", days: WEEKDAYS.map((day) => day.id) } : null,
                }))
              }
            />
            <span>Hold messages during quiet hours</span>
          </label>
          {quiet === null ? null : (
            <div className="stack">
              <div className="row">
                <TextField
                  label="Quiet period starts"
                  type="time"
                  value={quiet.start}
                  onChange={(event) => setDraft((current) => ({ ...current, quiet: current.quiet && { ...current.quiet, start: event.target.value } }))}
                  disabled={readOnly}
                />
                <TextField
                  label="Quiet period ends"
                  type="time"
                  value={quiet.end}
                  onChange={(event) => setDraft((current) => ({ ...current, quiet: current.quiet && { ...current.quiet, end: event.target.value } }))}
                  disabled={readOnly}
                  error={tried ? problems.quiet.end : undefined}
                />
              </div>
              <fieldset className="sending-days" aria-describedby={tried && problems.quiet.days ? "quiet-days-error" : undefined}>
                <legend className="field-label">Days a quiet period starts on</legend>
                {WEEKDAYS.map((day) => (
                  <label className="check" key={day.id}>
                    <input
                      type="checkbox"
                      checked={quiet.days.includes(day.id)}
                      disabled={readOnly}
                      onChange={(event) =>
                        setDraft((current) => ({
                          ...current,
                          quiet:
                            current.quiet && {
                              ...current.quiet,
                              days: event.target.checked ? [...current.quiet.days, day.id] : current.quiet.days.filter((entry) => entry !== day.id),
                            },
                        }))
                      }
                    />
                    <span>{day.label}</span>
                  </label>
                ))}
              </fieldset>
              {tried && problems.quiet.days ? (
                <p className="field-error" id="quiet-days-error">
                  {problems.quiet.days}
                </p>
              ) : null}
              <p className="muted" id="quiet-zone-note">
                The times are on the clock of the person you write to: their <span className="mono">timezone</span> field when it holds a time zone,
                otherwise the workspace zone{zone === null ? "" : ` (${zone})`}.
              </p>
            </div>
          )}
        </div>
      </Panel>

      {failure ? <InlineError {...failure} /> : null}
      {notice ? (
        <p className="notice" role="status">
          {notice}
        </p>
      ) : null}
      {readOnly ? (
        <p className="muted">Only workspace admins and the instance owner can change the sending limits.</p>
      ) : (
        <div className="row">
          <Button type="submit" variant="primary" disabled={busy || !dirty}>
            {busy ? "Saving" : "Save limits"}
          </Button>
          {dirty ? <span className="faint">Saving replaces the caps and quiet hours together.</span> : null}
        </div>
      )}
    </form>
  );
}
