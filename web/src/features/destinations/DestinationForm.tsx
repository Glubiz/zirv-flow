import { useState, type SyntheticEvent } from "react";
import { Link } from "react-router";

import { client, describeError, unwrap, type ErrorSummary } from "../../api";
import { Button, InlineError, SelectField, TextAreaField, TextField } from "../../ui";
import { CredentialPicker } from "../credentials";
import {
  configFrom,
  DESTINATION_KINDS,
  kindOf,
  valuesFrom,
  type Destination,
  type DestinationKind,
  type Values,
} from "./labels";

const MAX_NAME_CHARS = 128;
const MAX_RATE = 100000;

interface DestinationFormProps {
  workspaceId: string;
  /** The destination being changed; absent for a new one. */
  existing?: Destination;
  onSaved: (saved: Destination) => void;
}

function wholeInRange(text: string): boolean {
  const value = Number(text);
  return Number.isInteger(value) && value >= 1 && value <= MAX_RATE;
}

/** Creates or changes a destination: settings by type, the shared credential, the rate and the burst. */
export function DestinationForm({ workspaceId, existing, onSaved }: DestinationFormProps) {
  const initialKind = existing ? kindOf(existing.kind) : (DESTINATION_KINDS[0] ?? null);
  const [kind, setKind] = useState<DestinationKind | null>(initialKind);
  const [name, setName] = useState(existing?.name ?? "");
  const [values, setValues] = useState<Values>(initialKind && existing ? valuesFrom(initialKind, existing.config) : {});
  const [credentialId, setCredentialId] = useState(existing?.credential_id ?? "");
  const [rate, setRate] = useState(existing ? String(existing.rate) : "");
  const [burst, setBurst] = useState(existing ? String(existing.burst) : "");
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [busy, setBusy] = useState(false);

  function choose(next: string) {
    const chosen = kindOf(next);
    setKind(chosen);
    setValues({});
    setCredentialId("");
    setErrors({});
  }

  const needsCredential = kind?.credential.some((field) => field.required) ?? false;
  const rateOk = rate === "" ? existing === undefined : wholeInRange(rate);
  const burstOk = burst === "" ? existing === undefined : wholeInRange(burst);
  const ready =
    kind !== null && name.trim() !== "" && name.trim().length <= MAX_NAME_CHARS && (!needsCredential || credentialId !== "") && rateOk && burstOk;

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    if (!kind) return;
    const parsed = configFrom(kind, values);
    if (!parsed.ok) {
      setErrors(parsed.errors);
      return;
    }
    setErrors({});
    setBusy(true);
    setFailure(null);
    try {
      const shared = {
        name: name.trim(),
        config: parsed.config,
        ...(credentialId !== "" ? { credential_id: credentialId } : {}),
        ...(rate !== "" ? { rate: Number(rate) } : {}),
        ...(burst !== "" ? { burst: Number(burst) } : {}),
      };
      const saved = existing
        ? await unwrap(
            client.PATCH("/api/v1/workspaces/{ws}/destinations/{id}", {
              params: { path: { ws: workspaceId, id: existing.id } },
              body: shared,
            }),
          )
        : await unwrap(
            client.POST("/api/v1/workspaces/{ws}/destinations", {
              params: { path: { ws: workspaceId } },
              body: { ...shared, kind: kind.kind },
            }),
          );
      onSaved(saved);
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <form className="stack" onSubmit={(event) => void submit(event)} aria-label={existing ? "Change the destination" : "New destination"}>
      {existing ? (
        <p>
          <span className="strong">{kind?.label ?? existing.kind}</span> <span className="muted">The type cannot be changed.</span>
        </p>
      ) : (
        <SelectField
          label="Type"
          value={kind?.kind ?? ""}
          onChange={(event) => choose(event.target.value)}
          hint={kind?.description}
        >
          {DESTINATION_KINDS.map((entry) => (
            <option key={entry.kind} value={entry.kind}>
              {entry.label}
            </option>
          ))}
        </SelectField>
      )}
      <TextField
        label="Name"
        value={name}
        onChange={(event) => setName(event.target.value)}
        maxLength={MAX_NAME_CHARS}
        hint="Shown in the list of destinations. Two destinations cannot share a name."
        autoComplete="off"
      />
      {kind?.config.map((field) =>
        field.type === "long-text" || field.type === "json" ? (
          <div key={field.key} className="stack-tight">
            <TextAreaField
              label={field.required ? field.label : `${field.label} (optional)`}
              hint={field.help}
              mono
              rows={field.type === "json" ? 10 : 5}
              value={values[field.key] ?? ""}
              onChange={(event) => setValues({ ...values, [field.key]: event.target.value })}
            />
            {errors[field.key] ? <InlineError title={errors[field.key] ?? ""} /> : null}
          </div>
        ) : (
          <TextField
            key={field.key}
            label={field.required ? field.label : `${field.label} (optional)`}
            hint={field.help}
            type={field.type === "url" ? "url" : "text"}
            value={values[field.key] ?? ""}
            error={errors[field.key]}
            onChange={(event) => setValues({ ...values, [field.key]: event.target.value })}
            autoComplete="off"
          />
        ),
      )}
      {kind ? (
        <CredentialPicker
          key={kind.kind}
          workspaceId={workspaceId}
          fields={kind.credential}
          value={credentialId}
          onChange={setCredentialId}
          purpose={kind.label}
        />
      ) : null}
      <div className="grid-3">
        <TextField
          label="Rate (deliveries per second)"
          type="number"
          min={1}
          max={MAX_RATE}
          value={rate}
          onChange={(event) => setRate(event.target.value)}
          error={rate !== "" && !wholeInRange(rate) ? "A whole number from 1 to 100000." : undefined}
          hint={existing ? undefined : "Leave empty for the default of the type."}
        />
        <TextField
          label="Burst (most deliveries at once)"
          type="number"
          min={1}
          max={MAX_RATE}
          value={burst}
          onChange={(event) => setBurst(event.target.value)}
          error={burst !== "" && !wholeInRange(burst) ? "A whole number from 1 to 100000." : undefined}
          hint={existing ? undefined : "Leave empty to use the rate."}
        />
      </div>
      {failure ? <InlineError title={failure.title} detail={failure.detail} traceId={failure.traceId} /> : null}
      <div className="row">
        <Button type="submit" variant="primary" disabled={!ready || busy}>
          {existing ? "Save changes" : "Create destination"}
        </Button>
        {existing ? null : <Link to={`/w/${workspaceId}/destinations`}>Cancel</Link>}
      </div>
    </form>
  );
}
