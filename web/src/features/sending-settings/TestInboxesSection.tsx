import { useEffect, useRef, useState, type SyntheticEvent } from "react";
import { useLocation } from "react-router";

import { ApiError, client, describeError, unwrap, unwrapEmpty, useResource, type ErrorSummary } from "../../api";
import { Button, Dialog, EmptyState, ErrorState, InlineError, LoadingState, Panel, TextField } from "../../ui";
import { formatDateTime } from "../../ui/format";

export const TEST_INBOXES_ID = "test-inboxes";

interface Inbox {
  id: string;
  hint: string;
  /** Null when the reader may see the hint only. */
  addedAt: string | null;
}

interface Props {
  workspaceId: string;
  /** Admin and owner: add and remove. */
  canManage: boolean;
  /** Any member: the list, read-only. */
  canRead: boolean;
}

function refusal(error: unknown): ErrorSummary {
  if (error instanceof ApiError && error.status === 409) {
    return { title: "Already on the list", detail: "That address is already a test inbox.", traceId: error.traceId };
  }
  if (error instanceof ApiError && error.status === 422) {
    return { title: "Not an email address", detail: "That is not a valid email address.", traceId: error.traceId };
  }
  return describeError(error);
}

/** The "Test inboxes" section of the Sending screen: the addresses a test send may go to besides the team's. */
export function TestInboxesSection({ workspaceId, canManage, canRead }: Props) {
  const location = useLocation();
  const anchor = useRef<HTMLDivElement>(null);
  const inboxes = useResource(async (): Promise<Inbox[]> => {
    if (!canRead && !canManage) return [];
    if (canManage) {
      const body = await unwrap(client.GET("/api/v1/workspaces/{ws}/test-send-allowlist", { params: { path: { ws: workspaceId } } }));
      return body.entries.map((entry) => ({ id: entry.id, hint: entry.display_hint, addedAt: entry.created_at }));
    }
    const options = await unwrap(client.GET("/api/v1/workspaces/{ws}/test-send-allowlist/options", { params: { path: { ws: workspaceId } } }));
    return options.map((entry) => ({ id: entry.id, hint: entry.name, addedAt: null }));
  }, [workspaceId, canManage, canRead]);
  const [address, setAddress] = useState("");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [removing, setRemoving] = useState<Inbox | null>(null);

  const ready = inboxes.status === "ready";
  useEffect(() => {
    if (ready && location.hash === `#${TEST_INBOXES_ID}`) anchor.current?.scrollIntoView();
  }, [ready, location.hash]);

  async function add(event: SyntheticEvent) {
    event.preventDefault();
    if (busy || address.trim() === "") return;
    setBusy(true);
    setFailure(null);
    setNotice(null);
    try {
      const saved = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/test-send-allowlist", { params: { path: { ws: workspaceId } }, body: { address: address.trim() } }),
      );
      setAddress("");
      setNotice(`Added ${saved.display_hint}.`);
      inboxes.reload();
    } catch (error) {
      setFailure(refusal(error));
    } finally {
      setBusy(false);
    }
  }

  async function remove(entry: Inbox) {
    setBusy(true);
    setFailure(null);
    setNotice(null);
    try {
      await unwrapEmpty(
        client.DELETE("/api/v1/workspaces/{ws}/test-send-allowlist/{id}", { params: { path: { ws: workspaceId, id: entry.id } } }),
      );
      setNotice(`Removed ${entry.hint}.`);
      inboxes.reload();
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
      setRemoving(null);
    }
  }

  return (
    <div id={TEST_INBOXES_ID} ref={anchor}>
      <Panel title="Test inboxes">
        <div className="stack">
          <p className="muted">
            A test send goes only to a member of the team or to one of these addresses. Addresses are kept sealed; only the first letter and the
            domain show here.
          </p>
          {!canRead && !canManage ? <p className="muted">Only members of the workspace can see the test inboxes.</p> : null}
          {(canRead || canManage) && inboxes.status === "loading" ? <LoadingState label="Loading the test inboxes" /> : null}
          {inboxes.status === "error" ? <ErrorState {...describeError(inboxes.error)} onRetry={inboxes.reload} /> : null}
          {inboxes.status === "ready" && (canRead || canManage) && inboxes.data.length === 0 ? <EmptyState title="No test inboxes yet." /> : null}
          {inboxes.status === "ready" && inboxes.data.length > 0 ? (
            <ul className="records" aria-label="Test inboxes">
              {inboxes.data.map((entry) => (
                <li key={entry.id} className="row">
                  <span className="strong mono row-grow">{entry.hint}</span>
                  {entry.addedAt ? <span className="faint">Added {formatDateTime(entry.addedAt)}</span> : null}
                  {canManage ? (
                    <Button onClick={() => setRemoving(entry)}>
                      Remove <span className="visually-hidden">{entry.hint}</span>
                    </Button>
                  ) : null}
                </li>
              ))}
            </ul>
          ) : null}
          {canManage ? (
            <form className="stack" onSubmit={(event) => void add(event)} noValidate aria-label="Add a test inbox">
              <TextField
                label="Address"
                type="email"
                value={address}
                onChange={(event) => setAddress(event.target.value)}
                autoComplete="off"
                hint="It is shown masked once saved."
              />
              <div>
                <Button type="submit" variant="primary" disabled={busy || address.trim() === ""}>
                  {busy ? "Saving" : "Add address"}
                </Button>
              </div>
            </form>
          ) : canRead ? (
            <p className="muted">Only workspace admins and the instance owner can add or remove test inboxes.</p>
          ) : null}
          {failure ? <InlineError {...failure} /> : null}
          {notice ? (
            <p className="notice" role="status">
              {notice}
            </p>
          ) : null}
        </div>
      </Panel>
      {removing ? (
        <Dialog title="Remove this test inbox?" onClose={() => setRemoving(null)}>
          <p className="muted">
            <span className="mono">{removing.hint}</span> can no longer get tests. Tests already sent stay in the log.
          </p>
          <div className="dialog-actions">
            <Button onClick={() => setRemoving(null)}>Keep it</Button>
            <Button variant="primary" disabled={busy} onClick={() => void remove(removing)}>
              Remove
            </Button>
          </div>
        </Dialog>
      ) : null}
    </div>
  );
}
