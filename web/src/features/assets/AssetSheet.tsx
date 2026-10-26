import { useState } from "react";
import { Link } from "react-router";

import { ApiError, client, describeError, unwrap, unwrapEmpty, useResource, type ErrorSummary } from "../../api";
import { Button, CopyButton, Dialog, ErrorState, InlineError, LoadingState, Sheet } from "../../ui";
import { formatDate } from "../../ui/format";
import {
  absoluteUrl,
  dimensionsText,
  formatBytes,
  IN_USE_LINE,
  ownerKindLabel,
  ownerPath,
  shortHash,
  typeLabel,
  type Asset,
} from "./model";

interface Props {
  workspaceId: string;
  asset: Asset;
  canDelete: boolean;
  onClose: () => void;
  /** The asset is gone: the list should read again. */
  onDeleted: () => void;
}

/** The details of one image: its facts, its public link, where it is used, and Delete. */
export function AssetSheet({ workspaceId, asset, canDelete, onClose, onDeleted }: Props) {
  const detail = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/assets/{id}", { params: { path: { ws: workspaceId, id: asset.id } } })),
    [workspaceId, asset.id],
  );
  const [confirming, setConfirming] = useState(false);
  const [raced, setRaced] = useState(false);

  const owners = detail.status === "ready" ? detail.data.used_by : [];
  const inUse = raced || (detail.status === "ready" ? detail.data.used_by.length > 0 : asset.references > 0);

  return (
    <Sheet title={asset.name} closeLabel="Close details" onClose={onClose}>
      <div className="image-preview">
        <img src={asset.url} alt={asset.name} />
      </div>
      <dl className="image-facts">
        <dt>Type</dt>
        <dd>
          {typeLabel(asset.content_type)} · {dimensionsText(asset)}
        </dd>
        <dt>Size</dt>
        <dd>{formatBytes(asset.size_bytes)}</dd>
        <dt>Added</dt>
        <dd>{formatDate(asset.created_at)}</dd>
        <dt>SHA-256</dt>
        <dd className="mono" title={asset.sha256}>
          {shortHash(asset.sha256)}
        </dd>
      </dl>
      <div className="image-link">
        <span className="mono">{asset.url}</span>
        <CopyButton value={absoluteUrl(asset.url)} label="Copy link" />
      </div>
      <div className="stack-tight">
        <h3 className="strong">Used in</h3>
        {detail.status === "loading" ? <LoadingState label="Loading where it is used" /> : null}
        {detail.status === "error" ? <ErrorState {...describeError(detail.error)} onRetry={detail.reload} /> : null}
        {detail.status === "ready" && owners.length === 0 ? <p className="muted">Nothing uses this image yet.</p> : null}
        {owners.length > 0 ? (
          <ul className="image-owners">
            {owners.map((owner) => (
              <li key={`${owner.kind}:${owner.id}:${owner.version}`}>
                <Link to={ownerPath(workspaceId, owner)}>{owner.name ?? ownerKindLabel(owner.kind)}</Link>
                <span className="muted">
                  {owner.kind} · v{owner.version}
                  {owner.archived ? " · archived" : ""}
                </span>
              </li>
            ))}
          </ul>
        ) : null}
      </div>
      {canDelete ? (
        <div className="image-danger">
          <Button disabled={inUse} aria-describedby={inUse ? "image-in-use" : undefined} onClick={() => setConfirming(true)}>
            Delete
          </Button>
          {inUse ? (
            <span id="image-in-use" className="muted">
              {IN_USE_LINE}
            </span>
          ) : null}
        </div>
      ) : null}
      {confirming ? (
        <DeleteDialog
          workspaceId={workspaceId}
          asset={asset}
          onClose={() => setConfirming(false)}
          onInUse={() => {
            setConfirming(false);
            setRaced(true);
            detail.reload();
          }}
          onDeleted={onDeleted}
        />
      ) : null}
    </Sheet>
  );
}

interface DeleteProps {
  workspaceId: string;
  asset: Asset;
  onClose: () => void;
  onInUse: () => void;
  onDeleted: () => void;
}

function DeleteDialog({ workspaceId, asset, onClose, onInUse, onDeleted }: DeleteProps) {
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function remove() {
    setBusy(true);
    setFailure(null);
    try {
      await unwrapEmpty(client.DELETE("/api/v1/workspaces/{ws}/assets/{id}", { params: { path: { ws: workspaceId, id: asset.id } } }));
      onDeleted();
    } catch (error) {
      if (error instanceof ApiError && error.code === "asset_in_use") {
        onInUse();
        return;
      }
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title="Delete this image?" onClose={onClose}>
      <div className="stack">
        <p>
          <strong>{asset.name}</strong> is removed from the library and its file is deleted. This cannot be undone.
        </p>
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose} disabled={busy}>
            Cancel
          </Button>
          <Button variant="primary" onClick={() => void remove()} disabled={busy}>
            {busy ? "Deleting" : "Delete image"}
          </Button>
        </div>
      </div>
    </Dialog>
  );
}
