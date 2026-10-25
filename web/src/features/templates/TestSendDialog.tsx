import { useState, type SyntheticEvent } from "react";

import { client, describeError, unwrap, useResource, type components, type ErrorSummary, type Resource } from "../../api";
import { Button, Dialog, EmptyState, ErrorState, InlineError, LoadingState, Pill, SelectField, TextField } from "../../ui";
import { formatDateTime } from "../../ui/format";
import { useMembers } from "../members";
import type { Template, TestSend } from "./model";

interface Props {
  workspaceId: string;
  template: Template;
  /** Saves any pending edit first; resolves false when it could not. */
  flush: () => Promise<boolean>;
  /** The person whose values render the test, when an admin previews as one. */
  personId: string | null;
  onClose: () => void;
}

const OUTCOMES: Record<string, { label: string; tone: "neutral" | "attention" | "error" }> = {
  queued: { label: "Queued", tone: "neutral" },
  sent: { label: "Sent", tone: "neutral" },
  suppressed: { label: "Not sent, suppressed", tone: "attention" },
  sender_unverified: { label: "Not sent, sender not verified", tone: "error" },
  render_failed: { label: "Not sent, render failed", tone: "error" },
  unconfirmed: { label: "Unconfirmed", tone: "attention" },
  failed: { label: "Failed", tone: "error" },
};

function outcome(row: TestSend) {
  return OUTCOMES[row.outcome] ?? { label: row.outcome, tone: "neutral" as const };
}

export function TestSendDialog({ workspaceId, template, flush, personId, onClose }: Props) {
  const destinations = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/destinations", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const senders = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/sender-identities", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const members = useMembers(workspaceId);
  const allowlist = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/test-send-allowlist", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const log = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/test-sends", {
          params: { path: { ws: workspaceId }, query: { template_id: template.id, limit: 10 } },
        }),
      ),
    [workspaceId, template.id],
  );

  const [destination, setDestination] = useState("");
  const [sender, setSender] = useState(template.sender_identity_id ?? "");
  const [toKind, setToKind] = useState<"member" | "address">("member");
  const [member, setMember] = useState("");
  const [address, setAddress] = useState("");
  const [version, setVersion] = useState("draft");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [sentRow, setSentRow] = useState<TestSend | null>(null);

  const loading = [destinations, senders, members, allowlist].some((entry) => entry.status === "loading");
  const loadFailed = [destinations, senders, members, allowlist].find((entry) => entry.status === "error");
  const ready = !loading && !loadFailed;
  const incomplete = destination === "" || sender === "" || (toKind === "member" ? member === "" : address.trim() === "");

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    if (busy || incomplete) return;
    setBusy(true);
    setFailure(null);
    setSentRow(null);
    try {
      if (version === "draft" && !(await flush())) {
        setFailure({ title: "The draft could not be saved", detail: "Fix that, then send the test again.", traceId: null });
        setBusy(false);
        return;
      }
      const row = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/templates/{id}/test-send", {
          params: { path: { ws: workspaceId, id: template.id } },
          body: {
            destination_id: destination,
            sender_identity_id: sender,
            to: toKind === "member" ? { member } : { address: address.trim() },
            ...(version === "draft" ? {} : { version: Number(version) }),
            ...(personId ? { person_id: personId } : {}),
          },
        }),
      );
      setSentRow(row);
      log.reload();
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  function retryAll() {
    destinations.reload();
    senders.reload();
    members.reload();
    allowlist.reload();
  }

  return (
    <Dialog title="Send a test" wide onClose={onClose}>
      <div className="stack">
        <p className="muted">
          A test goes through the real destination, but only to a workspace member or an address on the allowlist. It does not count toward caps or
          engagement.
        </p>
        {loading ? <LoadingState label="Loading destinations and recipients" /> : null}
        {loadFailed?.status === "error" ? <ErrorState {...describeError(loadFailed.error)} onRetry={retryAll} /> : null}
        {ready ? (
          <form className="stack" onSubmit={(event) => void submit(event)} noValidate aria-label="Test send">
            <SelectField label="Destination" value={destination} onChange={(event) => setDestination(event.target.value)}>
              <option value="">Choose a destination</option>
              {destinations.status === "ready"
                ? destinations.data.map((entry) => (
                    <option key={entry.id} value={entry.id}>
                      {entry.name}
                    </option>
                  ))
                : null}
            </SelectField>
            <SelectField label="Sender identity" value={sender} onChange={(event) => setSender(event.target.value)}>
              <option value="">Choose a sender</option>
              {senders.status === "ready"
                ? senders.data.map((entry) => (
                    <option key={entry.id} value={entry.id}>
                      {entry.from_name} &lt;{entry.from_address}&gt;
                    </option>
                  ))
                : null}
            </SelectField>
            <SelectField label="Version" value={version} onChange={(event) => setVersion(event.target.value)}>
              <option value="draft">The current draft</option>
              {template.current_version ? <option value={String(template.current_version)}>Published version {template.current_version}</option> : null}
            </SelectField>
            <SelectField
              label="Send it to"
              value={toKind}
              onChange={(event) => setToKind(event.target.value === "address" ? "address" : "member")}
            >
              <option value="member">A workspace member</option>
              <option value="address">An allowlisted address</option>
            </SelectField>
            {toKind === "member" ? (
              <SelectField label="Member" value={member} onChange={(event) => setMember(event.target.value)}>
                <option value="">Choose a member</option>
                {members.status === "ready"
                  ? members.data.map((entry) => (
                      <option key={entry.user_id} value={entry.user_id}>
                        {entry.email}
                      </option>
                    ))
                  : null}
              </SelectField>
            ) : (
              <>
                <TextField
                  label="Address"
                  type="email"
                  value={address}
                  onChange={(event) => setAddress(event.target.value)}
                  autoComplete="off"
                  hint={
                    allowlist.status === "ready" && allowlist.data.entries.length > 0
                      ? `On the allowlist: ${allowlist.data.entries.map((entry) => entry.display_hint).join(", ")}`
                      : "The allowlist is empty. An admin adds addresses to it."
                  }
                />
              </>
            )}
            {personId ? <p className="faint">The test renders with the person you are previewing as.</p> : null}
            {failure ? <InlineError {...failure} /> : null}
            {sentRow ? (
              <p role="status" className="notice">
                Test queued. Its outcome is <span className="strong">{outcome(sentRow).label}</span> so far; the log below updates when you reload it.
              </p>
            ) : null}
            <div className="dialog-actions">
              <Button onClick={onClose}>Close</Button>
              <Button type="submit" variant="primary" disabled={busy || incomplete}>
                {busy ? "Sending" : "Send test"}
              </Button>
            </div>
          </form>
        ) : null}
        <TestSendLog log={log} />
      </div>
    </Dialog>
  );
}

function TestSendLog({ log }: { log: Resource<components["schemas"]["TestSendsBody"]> }) {
  return (
    <section aria-label="Test send log" className="stack">
      <div className="row">
        <h3 className="panel-title">Test send log</h3>
        <Button onClick={log.reload}>Reload the log</Button>
      </div>
      {log.status === "loading" ? <LoadingState label="Loading the test send log" /> : null}
      {log.status === "error" ? <ErrorState {...describeError(log.error)} onRetry={log.reload} /> : null}
      {log.status === "ready" && log.data.test_sends.length === 0 ? <EmptyState title="No tests sent yet" /> : null}
      {log.status === "ready" && log.data.test_sends.length > 0 ? (
        <div className="table-scroll">
          <table className="table">
            <caption>Recent tests of this template</caption>
            <thead>
              <tr>
                <th scope="col">When</th>
                <th scope="col">Version</th>
                <th scope="col">Outcome</th>
              </tr>
            </thead>
            <tbody>
              {log.data.test_sends.map((row) => (
                <tr key={row.id}>
                  <td>{formatDateTime(row.created_at)}</td>
                  <td>{row.template_version ? `Version ${row.template_version}` : "Draft"}</td>
                  <td>
                    <Pill tone={outcome(row).tone}>{outcome(row).label}</Pill>
                    {row.reason ? <span className="faint mono"> {row.reason}</span> : null}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : null}
    </section>
  );
}
