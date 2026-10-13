import { useState, type SyntheticEvent } from "react";
import { Link } from "react-router";

import { client, describeError, unwrap, type ErrorSummary } from "../../api";
import { Button, InlineError, SelectField, TextField } from "../../ui";
import type { CreatedWebhook } from "./labels";

const MAX_NAME_CHARS = 128;
const DEFAULT_SIGNATURE_HEADER = "X-Zirv-Signature";
const DEFAULT_PREFIX = "sha256=";
const DEFAULT_TIMESTAMP_HEADER = "X-Zirv-Timestamp";
const DEFAULT_TOLERANCE = "300";

interface NewWebhookFormProps {
  workspaceId: string;
  onCreated: (created: CreatedWebhook) => void;
}

/** Creates a webhook: a name, the payload field that identifies a payload, and how senders sign. */
export function NewWebhookForm({ workspaceId, onCreated }: NewWebhookFormProps) {
  const [name, setName] = useState("");
  const [dedupeField, setDedupeField] = useState("");
  const [expectMinutes, setExpectMinutes] = useState("");
  const [signatureHeader, setSignatureHeader] = useState(DEFAULT_SIGNATURE_HEADER);
  const [prefix, setPrefix] = useState(DEFAULT_PREFIX);
  const [timestamped, setTimestamped] = useState(true);
  const [timestampHeader, setTimestampHeader] = useState(DEFAULT_TIMESTAMP_HEADER);
  const [tolerance, setTolerance] = useState(DEFAULT_TOLERANCE);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  const nameOk = name.trim() !== "" && name.trim().length <= MAX_NAME_CHARS;
  const dedupeOk = dedupeField.trim() !== "" && !dedupeField.trim().split(".").includes("");
  const minutes = expectMinutes.trim() === "" ? null : Number(expectMinutes);
  const minutesOk = minutes === null || (Number.isInteger(minutes) && minutes > 0);
  const toleranceValue = Number(tolerance);
  const toleranceOk = !timestamped || (Number.isInteger(toleranceValue) && toleranceValue >= 1 && toleranceValue <= 86400);
  const headersOk =
    signatureHeader.trim() !== "" &&
    (!timestamped || (timestampHeader.trim() !== "" && timestampHeader.trim().toLowerCase() !== signatureHeader.trim().toLowerCase()));

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      const created = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/webhooks", {
          params: { path: { ws: workspaceId } },
          body: {
            name: name.trim(),
            dedupe_field: dedupeField.trim(),
            signature_header: signatureHeader.trim(),
            signature_prefix: prefix,
            timestamp_header: timestamped ? timestampHeader.trim() : null,
            tolerance_seconds: timestamped ? toleranceValue : undefined,
            staleness_threshold_seconds: minutes === null ? undefined : minutes * 60,
          },
        }),
      );
      onCreated(created);
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
      <p className="strong">Webhook</p>
      <p className="muted">
        A webhook gives you a URL and a secret. The system that sends data signs each request with the secret, and every payload is
        mapped to a person, an event and lists as you choose next.
      </p>
      <TextField
        label="Name"
        value={name}
        onChange={(event) => setName(event.target.value)}
        maxLength={MAX_NAME_CHARS}
        hint="Shown in the list of sources. Two sources cannot share a name."
        autoComplete="off"
      />
      <TextField
        label="Dedupe field"
        value={dedupeField}
        onChange={(event) => setDedupeField(event.target.value)}
        mono
        hint="The field of the payload that identifies it, such as id or data.order.id. Sending the same value again counts once."
        error={dedupeField !== "" && !dedupeOk ? "Use a dotted path without empty parts." : undefined}
        autoComplete="off"
      />
      <TextField
        label="Expect a payload at least every (minutes)"
        type="number"
        min={1}
        value={expectMinutes}
        onChange={(event) => setExpectMinutes(event.target.value)}
        hint="Optional. Without it the webhook never counts as stale."
        error={minutesOk ? undefined : "Use a whole number above zero."}
      />
      <details>
        <summary>Signature</summary>
        <div className="stack">
          <SelectField
            label="How the sender signs"
            value={timestamped ? "timestamped" : "body"}
            onChange={(event) => setTimestamped(event.target.value === "timestamped")}
            hint={
              timestamped
                ? "The sender signs the timestamp and the body, and a request outside the tolerance is refused."
                : "The sender signs the body alone. Nothing but the dedupe field stops a captured request from being sent again."
            }
          >
            <option value="timestamped">The timestamp and the body</option>
            <option value="body">The body only</option>
          </SelectField>
          <TextField
            label="Signature header"
            value={signatureHeader}
            onChange={(event) => setSignatureHeader(event.target.value)}
            mono
            autoComplete="off"
          />
          <TextField
            label="Text before the signature"
            value={prefix}
            onChange={(event) => setPrefix(event.target.value)}
            mono
            hint="The signature is 64 hex digits after this text. It may be empty."
            autoComplete="off"
          />
          {timestamped ? (
            <>
              <TextField
                label="Timestamp header"
                value={timestampHeader}
                onChange={(event) => setTimestampHeader(event.target.value)}
                mono
                hint="Unix seconds, as a decimal number."
                error={headersOk ? undefined : "Use a header name that differs from the signature header."}
                autoComplete="off"
              />
              <TextField
                label="Tolerance (seconds)"
                type="number"
                min={1}
                max={86400}
                value={tolerance}
                onChange={(event) => setTolerance(event.target.value)}
                error={toleranceOk ? undefined : "Use a whole number from 1 to 86400."}
              />
            </>
          ) : null}
        </div>
      </details>
      {failure ? <InlineError {...failure} /> : null}
      <div className="row">
        <Button type="submit" variant="primary" disabled={busy || !nameOk || !dedupeOk || !minutesOk || !toleranceOk || !headersOk}>
          {busy ? "Creating" : "Create webhook"}
        </Button>
        <Link to={`/w/${workspaceId}/sources`}>Cancel</Link>
      </div>
    </form>
  );
}
