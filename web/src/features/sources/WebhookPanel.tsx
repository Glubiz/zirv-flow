import { useState } from "react";
import { useNavigate } from "react-router";

import { client, describeError, unwrap, unwrapEmpty, useResource, type ErrorSummary } from "../../api";
import { Button, CopyButton, Dialog, EmptyState, formatDateTime, InlineError, Panel, Pill, ResourceView } from "../../ui";
import { EditWebhookDialog } from "./EditWebhookDialog";
import { FreshnessBadge } from "./FreshnessBadge";
import { lastSuccessLabel, schemeSummary, thresholdLabel, webhookUrl, type Stream, type Webhook } from "./labels";
import { WebhookSecretDialog } from "./WebhookSecretDialog";

interface WebhookPanelProps {
  workspaceId: string;
  sourceId: string;
  canManage: boolean;
  /** The webhook's one stream, for its freshness; null while the streams load. */
  stream: Stream | null;
  /** The webhook changed: the page reloads what it shows. */
  onChanged: (notice: string) => void;
}

/** The webhook of a source: where to send, how requests are signed, how fresh it is, and the admin actions. */
export function WebhookPanel({ workspaceId, sourceId, canManage, stream, onChanged }: WebhookPanelProps) {
  const hooks = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/webhooks", { params: { path: { ws: workspaceId } } })),
    [workspaceId, sourceId],
  );
  return (
    <Panel title="Webhook">
      <ResourceView
        resource={hooks}
        loadingLabel="Loading the webhook"
        isEmpty={(list) => !list.some((hook) => hook.source_id === sourceId)}
        empty={<EmptyState title="This source has no webhook">It may have been removed. Delete the source and create the webhook again.</EmptyState>}
      >
        {(list) => {
          const hook = list.find((entry) => entry.source_id === sourceId);
          return hook ? (
            <Detail
              workspaceId={workspaceId}
              hook={hook}
              stream={stream}
              canManage={canManage}
              onChanged={(notice) => {
                hooks.reload();
                onChanged(notice);
              }}
            />
          ) : null;
        }}
      </ResourceView>
    </Panel>
  );
}

interface DetailProps {
  workspaceId: string;
  hook: Webhook;
  stream: Stream | null;
  canManage: boolean;
  onChanged: (notice: string) => void;
}

function Detail({ workspaceId, hook, stream, canManage, onChanged }: DetailProps) {
  const navigate = useNavigate();
  const url = webhookUrl(window.location.origin, hook);
  const [editing, setEditing] = useState(false);
  const [confirmRotate, setConfirmRotate] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [rotated, setRotated] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const path = { params: { path: { ws: workspaceId, id: hook.id } } };

  async function rotate() {
    setBusy(true);
    setFailure(null);
    try {
      const fresh = await unwrap(client.POST("/api/v1/workspaces/{ws}/webhooks/{id}/rotate-secret", path));
      setConfirmRotate(false);
      setRotated(fresh.secret);
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  async function remove() {
    setBusy(true);
    setFailure(null);
    try {
      await unwrapEmpty(client.DELETE("/api/v1/workspaces/{ws}/webhooks/{id}", path));
      void navigate(`/w/${workspaceId}/sources`);
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <div className="stack">
      {!hook.receiving ? (
        <p className="notice" role="status">
          Not receiving: turn on the stream and set a mapping under Streams (Set up, then Map). Until then every request is answered
          with 404.
        </p>
      ) : null}
      <dl className="facts">
        <div className="fact">
          <dt>URL</dt>
          <dd className="row">
            <span className="mono">{url}</span>
            <CopyButton value={url} label="Copy the URL" />
          </dd>
        </div>
        <div className="fact">
          <dt>Status</dt>
          <dd>
            <Pill tone={hook.receiving ? "neutral" : "attention"}>{hook.receiving ? "Receiving" : "Not receiving"}</Pill>
          </dd>
        </div>
        <div className="fact">
          <dt>Freshness</dt>
          <dd>
            <FreshnessBadge state={stream?.freshness?.state} />
          </dd>
        </div>
        <div className="fact">
          <dt>Last received</dt>
          <dd title={stream?.freshness?.last_success_at ? formatDateTime(stream.freshness.last_success_at) : undefined}>
            {stream ? lastSuccessLabel(stream.freshness) : "—"}
          </dd>
        </div>
        <div className="fact">
          <dt>Stale after</dt>
          <dd>{stream ? thresholdLabel(stream.freshness) : "—"}</dd>
        </div>
        <div className="fact">
          <dt>Signature</dt>
          <dd>{schemeSummary(hook)}</dd>
        </div>
        <div className="fact">
          <dt>Dedupe field</dt>
          <dd className="mono">{hook.dedupe_field}</dd>
        </div>
        <div className="fact">
          <dt>Secret</dt>
          <dd>Stored encrypted. It was shown once, when it was issued.</dd>
        </div>
      </dl>
      {failure && !confirmRotate && !confirmDelete ? <InlineError {...failure} /> : null}
      {canManage ? (
        <div className="row">
          <Button onClick={() => setEditing(true)}>Edit webhook</Button>
          <Button
            onClick={() => {
              setFailure(null);
              setConfirmRotate(true);
            }}
          >
            Rotate secret
          </Button>
          <Button
            onClick={() => {
              setFailure(null);
              setConfirmDelete(true);
            }}
          >
            Delete webhook
          </Button>
        </div>
      ) : (
        <p className="muted">You can look at this webhook but not change it.</p>
      )}
      {editing ? (
        <EditWebhookDialog
          workspaceId={workspaceId}
          hook={hook}
          onClose={() => setEditing(false)}
          onSaved={() => {
            setEditing(false);
            onChanged(`Saved ${hook.name}.`);
          }}
        />
      ) : null}
      {confirmRotate ? (
        <Dialog title={`Rotate the secret of ${hook.name}?`} onClose={() => setConfirmRotate(false)}>
          <p className="muted">
            The current secret stops working at once, so the sender is refused until you give it the new one.
          </p>
          {failure ? <InlineError {...failure} /> : null}
          <div className="dialog-actions">
            <Button onClick={() => setConfirmRotate(false)}>Keep the secret</Button>
            <Button variant="primary" onClick={() => void rotate()} disabled={busy}>
              {busy ? "Rotating" : "Rotate secret"}
            </Button>
          </div>
        </Dialog>
      ) : null}
      {rotated !== null ? (
        <WebhookSecretDialog
          title="New secret"
          hook={hook}
          secret={rotated}
          onClose={() => {
            setRotated(null);
            onChanged("Issued a new secret.");
          }}
        />
      ) : null}
      {confirmDelete ? (
        <Dialog title={`Delete ${hook.name}?`} onClose={() => setConfirmDelete(false)}>
          <p className="muted">
            Deleting <span className="strong">{hook.name}</span> removes its URL, its secret, its stream and any payloads not yet
            applied. People and events it already created stay. This cannot be undone.
          </p>
          {failure ? <InlineError {...failure} /> : null}
          <div className="dialog-actions">
            <Button onClick={() => setConfirmDelete(false)}>Keep webhook</Button>
            <Button variant="primary" onClick={() => void remove()} disabled={busy}>
              {busy ? "Deleting" : `Delete ${hook.name}`}
            </Button>
          </div>
        </Dialog>
      ) : null}
    </div>
  );
}
