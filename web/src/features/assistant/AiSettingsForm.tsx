import { useState, type SyntheticEvent } from "react";

import {
  ApiError,
  client,
  describeError,
  unwrap,
  type components,
  type ErrorSummary,
  type FieldError,
} from "../../api";
import { Button, formatDateTime, InlineError, Panel, Pill, SelectField, TextField } from "../../ui";
import { fromMicros, toMicros } from "./money";

type Models = components["schemas"]["AiModelsBody"];
type Settings = components["schemas"]["AiSettingsBody"];

/** The window offered when transcripts are first turned on. */
const DEFAULT_TRANSCRIPT_DAYS = "7";

const CAPABILITIES: Record<string, string> = {
  tool_calls: "Tool calls",
  usage: "Token usage",
  context_window: "Context window",
};

interface AiSettingsFormProps {
  workspaceId: string;
  models: Models;
  settings: Settings;
  onSaved: () => void;
}

interface Failure {
  summary: ErrorSummary;
  /** The capabilities the chosen model lacks, when the server ran the probe and it failed. */
  capabilities: FieldError[];
}

function failureOf(error: unknown): Failure {
  const capabilities = error instanceof ApiError && error.code === "ai_capability_missing" ? error.errors : [];
  return { summary: describeError(error), capabilities };
}

export function AiSettingsForm({ workspaceId, models, settings, onSaved }: AiSettingsFormProps) {
  const [enabled, setEnabled] = useState(settings.enabled);
  const [providerId, setProviderId] = useState(settings.provider_id ?? "");
  const [model, setModel] = useState(settings.model ?? "");
  const [key, setKey] = useState("");
  const [budget, setBudget] = useState(fromMicros(settings.monthly_budget_micros));
  const [pii, setPii] = useState(settings.pii_opt_in);
  const [keepTranscripts, setKeepTranscripts] = useState(settings.transcript_days != null);
  const [days, setDays] = useState(String(settings.transcript_days ?? DEFAULT_TRANSCRIPT_DAYS));
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<Failure | null>(null);

  const provider = models.providers.find((entry) => entry.id === providerId);
  const chosen = provider?.models.find((entry) => entry.name === model);
  const budgetMicros = toMicros(budget);
  const dayCount = Number(days);
  const daysValid = Number.isInteger(dayCount) && dayCount >= 1 && dayCount <= 30;
  const needsKey = enabled && key.trim() === "" && !settings.provider_key_set;
  const incomplete = enabled && (!provider || !chosen);
  const invalid = budgetMicros === null || (keepTranscripts && !daysValid) || needsKey || incomplete;

  function chooseProvider(id: string) {
    setProviderId(id);
    setModel(models.providers.find((entry) => entry.id === id)?.models[0]?.name ?? "");
  }

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    if (budgetMicros === null) return;
    setBusy(true);
    setFailure(null);
    try {
      await unwrap(
        client.PUT("/api/v1/workspaces/{ws}/ai/settings", {
          params: { path: { ws: workspaceId } },
          body: {
            enabled,
            provider_id: providerId === "" ? null : providerId,
            model: model === "" ? null : model,
            monthly_budget_micros: budgetMicros,
            pii_opt_in: pii,
            transcript_days: keepTranscripts ? dayCount : null,
            provider_key: key.trim() === "" ? null : key,
          },
        }),
      );
      onSaved();
    } catch (error) {
      setFailure(failureOf(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
      <Panel title="Provider and model">
        <div className="stack">
          <label className="check">
            <input type="checkbox" checked={enabled} onChange={(event) => setEnabled(event.target.checked)} />
            <span>
              Turn the assistant on{" "}
              <span className="faint">
                Saving runs a short check that the model can call tools and reports its token use.
              </span>
            </span>
          </label>
          <SelectField label="Provider" value={providerId} onChange={(event) => chooseProvider(event.target.value)}>
            <option value="">Choose a provider</option>
            {models.providers.map((entry) => (
              <option key={entry.id} value={entry.id}>
                {entry.id}
              </option>
            ))}
          </SelectField>
          <SelectField
            label="Model"
            value={model}
            onChange={(event) => setModel(event.target.value)}
            disabled={!provider}
            hint={
              chosen
                ? `Context window: ${chosen.context_window_tokens.toLocaleString("en-US")} tokens.${
                    chosen.meets_minimum_context
                      ? ""
                      : ` Below the ${models.min_context_window_tokens.toLocaleString("en-US")} the assistant needs, so it cannot be turned on.`
                  }`
                : undefined
            }
          >
            <option value="">Choose a model</option>
            {provider?.models.map((entry) => (
              <option key={entry.name} value={entry.name}>
                {entry.name}
              </option>
            ))}
          </SelectField>
          <div className="stack">
            <TextField
              label="Provider key"
              type="password"
              autoComplete="new-password"
              value={key}
              onChange={(event) => setKey(event.target.value)}
              hint={
                settings.provider_key_set
                  ? "A key is stored. Leave this empty to keep it, or type a new key to replace it."
                  : "Stored encrypted and never shown again. A local model server that needs no key still takes any text here."
              }
              error={needsKey ? "A key is needed to turn the assistant on." : undefined}
            />
            {settings.provider_key_set ? <Pill>Key set</Pill> : <Pill tone="error">No key</Pill>}
          </div>
          {settings.last_probe ? (
            <p className="muted">
              {settings.last_probe.ok ? "The last check passed" : "The last check failed"} on{" "}
              {formatDateTime(settings.last_probe.at)}.
            </p>
          ) : null}
        </div>
      </Panel>
      <Panel title="Spending">
        <TextField
          label="Monthly budget"
          type="number"
          inputMode="decimal"
          min="0"
          step="0.01"
          hint="In your deployment's currency, per calendar month (UTC). With 0 the assistant makes no calls."
          value={budget}
          onChange={(event) => setBudget(event.target.value)}
          error={budgetMicros === null ? "Enter an amount of 0 or more." : undefined}
        />
      </Panel>
      <Panel title="Privacy">
        <div className="stack">
          <label className="check">
            <input type="checkbox" checked={pii} onChange={(event) => setPii(event.target.checked)} />
            <span>
              Allow personal data in requests{" "}
              <span className="faint">
                Off, the assistant sees only field names, types and counts. On, it may also see identifiers and property
                values of people, and sends them to the provider.
              </span>
            </span>
          </label>
          <label className="check">
            <input
              type="checkbox"
              checked={keepTranscripts}
              onChange={(event) => setKeepTranscripts(event.target.checked)}
            />
            <span>
              Keep transcripts{" "}
              <span className="faint">
                Off, requests and answers are not stored; only a log of counts and names is kept. On, they are stored for
                the days below and then deleted.
              </span>
            </span>
          </label>
          {keepTranscripts ? (
            <TextField
              label="Days to keep transcripts"
              type="number"
              inputMode="numeric"
              min="1"
              max="30"
              value={days}
              onChange={(event) => setDays(event.target.value)}
              error={daysValid ? undefined : "Enter a whole number from 1 to 30."}
            />
          ) : null}
        </div>
      </Panel>
      {failure ? (
        <div className="stack" role="alert">
          <InlineError {...failure.summary} />
          {failure.capabilities.length > 0 ? (
            <ul aria-label="Missing capabilities">
              {failure.capabilities.map((entry) => (
                <li key={entry.field}>
                  <span className="strong">{CAPABILITIES[entry.field] ?? entry.field}:</span> {entry.reason}
                </li>
              ))}
            </ul>
          ) : null}
        </div>
      ) : null}
      <div className="form-actions">
        <Button type="submit" variant="primary" disabled={busy || invalid}>
          {busy ? "Saving" : "Save settings"}
        </Button>
      </div>
    </form>
  );
}
