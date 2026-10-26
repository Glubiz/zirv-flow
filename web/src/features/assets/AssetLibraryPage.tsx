import { useRef, useState, type ChangeEvent, type DragEvent } from "react";
import { useParams } from "react-router";

import { Button, EmptyState, InlineError, LibraryNav, ResourceView } from "../../ui";
import { useAssetAccess } from "./access";
import { AssetSheet } from "./AssetSheet";
import { ACCEPTED_TYPES, dimensionsText, formatBytes, matchesSearch, type Asset } from "./model";
import { Thumb, UploadIcon } from "./Thumb";
import { useAssets } from "./useAssets";
import { useAssetUpload, type Sending } from "./useAssetUpload";

export function AssetLibraryPage() {
  const { ws = "" } = useParams();
  return <Library key={ws} workspaceId={ws} />;
}

function Library({ workspaceId }: { workspaceId: string }) {
  const access = useAssetAccess(workspaceId);
  const { resource, hasMore, loadingMore, loadMoreFailed, loadMore, add, reload, forget } = useAssets(workspaceId);
  const maxBytes = resource.status === "ready" ? resource.data.maxFileBytes : 5 * 1024 * 1024;
  const { sending, failure, upload } = useAssetUpload(workspaceId, maxBytes);
  const [search, setSearch] = useState("");
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [dragging, setDragging] = useState(false);
  const chooser = useRef<HTMLInputElement>(null);

  async function send(file: File | undefined) {
    if (!file) return;
    const asset = await upload(file);
    if (asset) {
      add(asset);
      setSelectedId(asset.id);
      reload();
    }
  }

  function onChoose(event: ChangeEvent<HTMLInputElement>) {
    void send(event.target.files?.[0]);
    event.target.value = "";
  }

  function onDrop(event: DragEvent<HTMLDivElement>) {
    event.preventDefault();
    setDragging(false);
    if (access.canWrite) void send(event.dataTransfer.files[0]);
  }

  const used = resource.status === "ready" ? resource.data : null;
  const percent = used && used.limitBytes > 0 ? Math.min(100, Math.round((used.usedBytes / used.limitBytes) * 100)) : 0;
  const selected = resource.status === "ready" ? resource.data.assets.find((asset) => asset.id === selectedId) : undefined;

  return (
    <div className="page">
      <LibraryNav workspaceId={workspaceId} />
      <div className="images-layout">
        <div
          className={dragging ? "images-main image-drop" : "images-main"}
          onDragOver={(event) => {
            if (!access.canWrite) return;
            event.preventDefault();
            setDragging(true);
          }}
          onDragLeave={() => setDragging(false)}
          onDrop={onDrop}
        >
          <div className="images-head">
            <h1 className="images-title">Images</h1>
            {used ? (
              <div className="meter">
                <span>
                  {formatBytes(used.usedBytes)} of {formatBytes(used.limitBytes)} used
                </span>
                <div
                  className="meter-track"
                  role="meter"
                  aria-label="Storage used"
                  aria-valuemin={0}
                  aria-valuemax={used.limitBytes}
                  aria-valuenow={used.usedBytes}
                  aria-valuetext={`${formatBytes(used.usedBytes)} of ${formatBytes(used.limitBytes)}`}
                >
                  <div className="meter-fill" style={{ width: `${percent}%` }} />
                </div>
              </div>
            ) : null}
            <input
              className="input images-search"
              type="search"
              placeholder="Search images"
              aria-label="Search images"
              value={search}
              onChange={(event) => setSearch(event.target.value)}
            />
            {access.canWrite ? (
              <>
                <input
                  ref={chooser}
                  className="visually-hidden"
                  type="file"
                  accept={ACCEPTED_TYPES}
                  aria-label="Choose an image file"
                  tabIndex={-1}
                  onChange={onChoose}
                />
                <Button variant="primary" disabled={sending !== null} onClick={() => chooser.current?.click()}>
                  <UploadIcon />
                  Upload
                </Button>
              </>
            ) : null}
          </div>
          {failure ? <InlineError {...failure} /> : null}
          <ResourceView
            resource={resource}
            loadingLabel="Loading images"
            isEmpty={(data) => data.assets.length === 0 && sending === null}
            empty={
              <EmptyState title="No images yet">
                {access.canWrite ? "Upload a PNG, JPEG, GIF or WebP, or drop it here." : "Members of the workspace upload the images."}
              </EmptyState>
            }
          >
            {(data) => {
              const shown = data.assets.filter((asset) => matchesSearch(asset, search));
              return (
                <>
                  <ul className="image-grid" aria-label="Images">
                    {sending ? <SendingTile sending={sending} /> : null}
                    {shown.map((asset) => (
                      <li key={asset.id}>
                        <AssetTile asset={asset} selected={asset.id === selectedId} onSelect={() => setSelectedId(asset.id)} />
                      </li>
                    ))}
                  </ul>
                  {shown.length === 0 ? <p className="muted">No image is named like that.</p> : null}
                  {hasMore ? (
                    <div>
                      <Button onClick={loadMore} disabled={loadingMore}>
                        {loadingMore ? "Loading" : "Show more"}
                      </Button>
                      {loadMoreFailed ? <span className="field-error"> Could not load more images. Try again.</span> : null}
                    </div>
                  ) : null}
                </>
              );
            }}
          </ResourceView>
        </div>
        {selected ? (
          <div className="images-side">
            <AssetSheet
              key={selected.id}
              workspaceId={workspaceId}
              asset={selected}
              canDelete={access.canAdmin}
              onClose={() => setSelectedId(null)}
              onDeleted={() => {
                forget(selected.id);
                setSelectedId(null);
                reload();
              }}
            />
          </div>
        ) : null}
      </div>
    </div>
  );
}

function AssetTile({ asset, selected, onSelect }: { asset: Asset; selected: boolean; onSelect: () => void }) {
  return (
    <button type="button" className="image-tile" aria-pressed={selected} onClick={onSelect}>
      <Thumb asset={asset} />
      <span className="image-name">{asset.name}</span>
      <span className="image-meta">
        {dimensionsText(asset)} · {formatBytes(asset.size_bytes)}
      </span>
    </button>
  );
}

export function SendingTile({ sending }: { sending: Sending }) {
  const percent = sending.total > 0 ? Math.round((sending.sent / sending.total) * 100) : 0;
  return (
    <li>
      <button type="button" className="image-tile" disabled>
        <span className="image-thumb" aria-hidden="true">
          <UploadIcon />
        </span>
        <span className="image-name">{sending.name}</span>
        <span className="image-meta" role="status">
          Uploading {percent}%
        </span>
      </button>
    </li>
  );
}
