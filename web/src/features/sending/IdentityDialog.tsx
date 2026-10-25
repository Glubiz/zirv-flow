import { useState } from "react";

import { client, describeError, unwrap, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError, TextField } from "../../ui";
import type { SenderIdentity, SendingDomain } from "./labels";

const MAX_NAME_CHARS = 128;

interface IdentityDialogProps {
  workspaceId: string;
  domain: SendingDomain;
  existing?: SenderIdentity;
  onClose: () => void;
  onSaved: () => void;
}

/** Adds a sender identity on a domain, or changes one: its display name, address and reply-to. */
export function IdentityDialog({ workspaceId, domain, existing, onClose, onSaved }: IdentityDialogProps) {
  const [fromName, setFromName] = useState(existing?.from_name ?? "");
  const [fromAddress, setFromAddress] = useState(existing?.from_address ?? "");
  const [replyTo, setReplyTo] = useState(existing?.reply_to ?? "");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function save() {
    setBusy(true);
    setFailure(null);
    try {
      if (existing) {
        await unwrap(
          client.PATCH("/api/v1/workspaces/{ws}/sender-identities/{id}", {
            params: { path: { ws: workspaceId, id: existing.id } },
            body: {
              from_name: fromName.trim(),
              from_address: fromAddress.trim(),
              reply_to: replyTo.trim() === "" ? null : replyTo.trim(),
            },
          }),
        );
      } else {
        await unwrap(
          client.POST("/api/v1/workspaces/{ws}/sender-identities", {
            params: { path: { ws: workspaceId } },
            body: {
              domain_id: domain.id,
              from_name: fromName.trim(),
              from_address: fromAddress.trim(),
              ...(replyTo.trim() !== "" ? { reply_to: replyTo.trim() } : {}),
            },
          }),
        );
      }
      onSaved();
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  const ready = fromName.trim() !== "" && fromName.trim().length <= MAX_NAME_CHARS && fromAddress.trim() !== "";

  return (
    <Dialog title={existing ? `Change ${existing.from_name}` : `New sender identity on ${domain.domain}`} onClose={onClose}>
      <form
        className="stack"
        onSubmit={(event) => {
          event.preventDefault();
          void save();
        }}
      >
        <TextField
          label="From name"
          value={fromName}
          onChange={(event) => setFromName(event.target.value)}
          maxLength={MAX_NAME_CHARS}
          hint="The display name recipients see."
          autoComplete="off"
        />
        <TextField
          label="From address"
          type="email"
          value={fromAddress}
          onChange={(event) => setFromAddress(event.target.value)}
          hint={`An address on ${domain.domain}.`}
          autoComplete="off"
        />
        <TextField
          label="Reply-to (optional)"
          type="email"
          value={replyTo}
          onChange={(event) => setReplyTo(event.target.value)}
          hint="Where replies go, if not to the from address. May be on any domain."
          autoComplete="off"
        />
        {failure ? <InlineError title={failure.title} detail={failure.detail} traceId={failure.traceId} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button type="submit" variant="primary" disabled={!ready || busy}>
            {existing ? "Save changes" : "Add the identity"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
