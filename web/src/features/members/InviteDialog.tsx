import { useState, type SyntheticEvent } from "react";

import { client, describeError, unwrap, type components, type ErrorSummary } from "../../api";
import { Button, CopyButton, Dialog, InlineError, SelectField, TextField } from "../../ui";
import { ROLE_OPTIONS, roleFrom } from "./roles";
import type { MemberRole } from "./useWorkspaceAccess";

type Created = components["schemas"]["CreatedInvitationBody"];

const EXPIRY_OPTIONS = [
  { value: "1", label: "1 day" },
  { value: "7", label: "7 days" },
  { value: "30", label: "30 days" },
];

/** The link as a person can use it: a server without a public URL answers a path. */
function absolute(link: string): string {
  return link.startsWith("/") ? `${globalThis.location.origin}${link}` : link;
}

interface InviteDialogProps {
  workspaceId: string;
  onClose: () => void;
  onCreated: () => void;
}

export function InviteDialog({ workspaceId, onClose, onCreated }: InviteDialogProps) {
  const [email, setEmail] = useState("");
  const [role, setRole] = useState<MemberRole>("viewer");
  const [days, setDays] = useState("7");
  const [viewPii, setViewPii] = useState(false);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [created, setCreated] = useState<Created | null>(null);

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      setCreated(
        await unwrap(
          client.POST("/api/v1/workspaces/{ws}/invitations", {
            params: { path: { ws: workspaceId } },
            body: { email: email.trim(), role, view_pii: role !== "admin" && viewPii, expires_in_days: Number(days) },
          }),
        ),
      );
      onCreated();
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  if (created) {
    const link = absolute(created.link);
    return (
      <Dialog key="link" title="Invite" onClose={onClose}>
        <TextField label="Invitation link" mono readOnly value={link} onFocus={(event) => event.currentTarget.select()} />
        <div className="dialog-actions">
          <CopyButton value={link} label="Copy the invitation link" />
          <Button variant="primary" onClick={onClose}>
            Done
          </Button>
        </div>
      </Dialog>
    );
  }

  return (
    <Dialog key="form" title="Invite" onClose={busy ? () => undefined : onClose}>
      <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
        <TextField
          label="Email"
          type="email"
          autoComplete="off"
          value={email}
          onChange={(event) => setEmail(event.target.value)}
          required
        />
        <SelectField label="Role" value={role} onChange={(event) => setRole(roleFrom(event.target.value))}>
          {ROLE_OPTIONS.map((option) => (
            <option key={option.value} value={option.value}>
              {option.label}
            </option>
          ))}
        </SelectField>
        <SelectField label="Expires" value={days} onChange={(event) => setDays(event.target.value)}>
          {EXPIRY_OPTIONS.map((option) => (
            <option key={option.value} value={option.value}>
              {option.label}
            </option>
          ))}
        </SelectField>
        <label className="check">
          <input
            type="checkbox"
            checked={role === "admin" || viewPii}
            disabled={role === "admin"}
            onChange={(event) => setViewPii(event.target.checked)}
          />
          <span>Personal data</span>
        </label>
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose} disabled={busy}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" disabled={busy || email.trim() === ""}>
            {busy ? "Creating" : "Create invite"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
