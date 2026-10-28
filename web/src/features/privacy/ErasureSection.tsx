import { useCallback, useState, type SyntheticEvent } from "react";

import { client, describeError, unwrap, type ErrorSummary } from "../../api";
import {
  Button,
  Dialog,
  EmptyState,
  formatDateTime,
  InlineError,
  Panel,
  Pill,
  ResourceView,
  shortId,
  TextField,
} from "../../ui";
import { CONFIRM_PHRASE, erasureStatus } from "./labels";
import { TargetForm, type PersonTarget } from "./TargetForm";
import { ShowMore } from "./ShowMore";
import { usePaged } from "./usePaged";
import { usePolling } from "./usePolling";

const PAGE_SIZE = 50;

export function ErasureSection({ workspaceId }: { workspaceId: string }) {
  const fetchPage = useCallback(
    async (before: string | null) => {
      const page = await unwrap(
        client.GET("/api/v1/workspaces/{ws}/privacy/erasures", {
          params: { path: { ws: workspaceId }, query: { limit: PAGE_SIZE, cursor: before ?? undefined } },
        }),
      );
      return { items: page.erasures, next: page.next_cursor ?? null };
    },
    [workspaceId],
  );
  const requests = usePaged(fetchPage, [workspaceId]);
  usePolling(
    requests.resource.status === "ready" && requests.resource.data.some((request) => request.status === "pending"),
    requests.resource.status === "ready" && requests.resource.refreshing,
    requests.resource.reload,
  );
  const [pending, setPending] = useState<PersonTarget | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  function requested() {
    setPending(null);
    setNotice("Erasure requested. It runs in the background and the list shows when it is done.");
    requests.resource.reload();
  }

  return (
    <>
      <Panel>
        <p className="muted">
          Use this for a data-subject request. It deletes the person and all their data. Their email and phone stay
          suppressed so they are never contacted again, and a ledger keeps only one-way hashes so the erasure can be applied
          again after a backup is restored.
        </p>
        <TargetForm legend="Erase a person" submitLabel="Erase…" busy={false} onSubmit={setPending} />
        {notice ? (
          <p className="notice" role="status">
            {notice}
          </p>
        ) : null}
      </Panel>
      <Panel title="Erasure requests" flush>
        {requests.refreshError ? <InlineError {...describeError(requests.refreshError)} /> : null}
        <ResourceView
          resource={requests.resource}
          loadingLabel="Loading erasure requests"
          isEmpty={(data) => data.length === 0}
          empty={<EmptyState title="No erasure has been requested">Requests appear here with their status.</EmptyState>}
        >
          {(data) => (
            <div className="table-scroll">
              <table className="table">
                <caption>Erasure requests</caption>
                <thead>
                  <tr>
                    <th scope="col">Person</th>
                    <th scope="col">Status</th>
                    <th scope="col">Requested</th>
                    <th scope="col">Completed</th>
                  </tr>
                </thead>
                <tbody>
                  {data.map((request) => {
                    const status = erasureStatus(request.status);
                    return (
                      <tr key={request.id}>
                        <td className="mono" title={request.person_id}>
                          {shortId(request.person_id)}
                        </td>
                        <td>
                          <Pill tone={status.tone}>{status.label}</Pill>
                          {request.status === "failed" ? (
                            <div className="faint">
                              Trace ID <span className="mono">{request.trace_id}</span>
                            </div>
                          ) : null}
                        </td>
                        <td>{formatDateTime(request.requested_at)}</td>
                        <td>{request.completed_at ? formatDateTime(request.completed_at) : "—"}</td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
        </ResourceView>
        <ShowMore paged={requests} />
      </Panel>
      {pending ? (
        <ConfirmErasureDialog workspaceId={workspaceId} target={pending} onClose={() => setPending(null)} onRequested={requested} />
      ) : null}
    </>
  );
}

interface ConfirmErasureDialogProps {
  workspaceId: string;
  target: PersonTarget;
  onClose: () => void;
  onRequested: () => void;
}

function ConfirmErasureDialog({ workspaceId, target, onClose, onRequested }: ConfirmErasureDialogProps) {
  const [typed, setTyped] = useState("");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    if (typed !== CONFIRM_PHRASE) return;
    setBusy(true);
    setFailure(null);
    try {
      await unwrap(
        client.POST("/api/v1/workspaces/{ws}/privacy/erasures", { params: { path: { ws: workspaceId } }, body: target }),
      );
      onRequested();
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title="Erase this person?" onClose={onClose}>
      <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
        <p>
          This deletes the person and all their data: profile, identities, events, consent, list memberships and their
          history. Their email and phone stay suppressed. It cannot be undone.
        </p>
        <TextField
          label={`Type ${CONFIRM_PHRASE} to confirm`}
          value={typed}
          onChange={(event) => setTyped(event.target.value)}
          autoComplete="off"
          required
        />
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button type="submit" variant="primary" disabled={busy || typed !== CONFIRM_PHRASE}>
            {busy ? "Requesting" : "Erase the person"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
