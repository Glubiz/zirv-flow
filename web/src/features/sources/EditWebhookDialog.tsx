import { useState, type SyntheticEvent } from "react";

import { client, describeError, unwrap, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError, SelectField, TextField } from "../../ui";
import type { Webhook } from "./labels";

interface EditWebhookDialogProps {
  workspaceId: string;
  hook: Webhook;
  onClose: () => void;
  onSaved: () => void;
}

/** Changes a webhook's name, dedupe field and signature scheme. The secret is not here; rotate it instead. */
export function EditWebhookDialog({ workspaceId, hook, onClose, onSaved }: EditWebhookDialogProps) {
  const [name, setName] = useState(hook.name);
  const [dedupeField, setDedupeField] = useState(hook.dedupe_field);
  const [signatureHeader, setSignatureHeader] = useState(hook.signature_header);
  const [prefix, setPrefix] = useState(hook.signature_prefix);
  const [timestamped, setTimestamped] = useState(hook.timestamp_header !== null);
  const [timestampHeader, setTimestampHeader] = useState(hook.timestamp_header ?? "X-Zirv-Timestamp");
  const [tolerance, setTolerance] = useState(String(hook.tolerance_seconds));
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  const nameOk = name.trim() !== "" && name.trim().length <= 128;
  const dedupeOk = dedupeField.trim() !== "" && !dedupeField.trim().split(".").includes("");
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
      await unwrap(
        client.PATCH("/api/v1/workspaces/{ws}/webhooks/{id}", {
          params: { path: { ws: workspaceId, id: hook.id } },
          body: {
            name: name.trim(),
            dedupe_field: dedupeField.trim(),
            signature_header: signatureHeader.trim(),
            signature_prefix: prefix,
            timestamp_header: timestamped ? timestampHeader.trim() : null,
            tolerance_seconds: timestamped ? toleranceValue : undefined,
          },
        }),
      );
      onSaved();
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title={`Edit ${hook.name}`} onClose={onClose}>
      <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
        <p className="muted">A change applies to the next request. A sender that still signs the old way is refused until it is updated.</p>
        <TextField label="Name" value={name} onChange={(event) => setName(event.target.value)} maxLength={128} autoComplete="off" />
        <TextField
          label="Dedupe field"
          value={dedupeField}
          onChange={(event) => setDedupeField(event.target.value)}
          mono
          error={dedupeOk ? undefined : "Use a dotted path without empty parts."}
          autoComplete="off"
        />
        <SelectField
          label="How the sender signs"
          value={timestamped ? "timestamped" : "body"}
          onChange={(event) => setTimestamped(event.target.value === "timestamped")}
        >
          <option value="timestamped">The timestamp and the body</option>
          <option value="body">The body only</option>
        </SelectField>
        <TextField label="Signature header" value={signatureHeader} onChange={(event) => setSignatureHeader(event.target.value)} mono autoComplete="off" />
        <TextField label="Text before the signature" value={prefix} onChange={(event) => setPrefix(event.target.value)} mono autoComplete="off" />
        {timestamped ? (
          <>
            <TextField
              label="Timestamp header"
              value={timestampHeader}
              onChange={(event) => setTimestampHeader(event.target.value)}
              mono
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
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button type="submit" variant="primary" disabled={busy || !nameOk || !dedupeOk || !toleranceOk || !headersOk}>
            {busy ? "Saving" : "Save webhook"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
