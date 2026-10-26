import { useRef, useState, type ChangeEvent, type KeyboardEvent } from "react";
import { useParams } from "react-router";

import { Button, Dialog, EmptyState, InlineError, ResourceView, TextField } from "../../ui";
import { useAssetAccess } from "./access";
import { ACCEPTED_TYPES, DEFAULT_MAX_FILE_BYTES, dimensionsText, matchesSearch } from "./model";
import { SendingTile, Thumb, UploadIcon } from "./Thumb";
import { useAssets } from "./useAssets";
import { useAssetUpload } from "./useAssetUpload";

/** What the person chose: a library image, the logo of the theme in effect, or an https address. */
export type AssetPick = { asset: { id: string; url: string } } | { themeLogo: true } | { url: string };

interface Props {
  open: boolean;
  onPick: (pick: AssetPick) => void;
  onClose: () => void;
  /** Offer the "Theme logo" choice: pass it when a theme is in effect. */
  themeLogo?: boolean;
}

const THEME_LOGO = "theme-logo";

/** The https address typed, as the address to use; null when it is not one. */
export function httpsAddress(text: string): string | null {
  try {
    const url = new URL(text.trim());
    return url.protocol === "https:" && url.hostname !== "" ? url.href : null;
  } catch {
    return null;
  }
}

export function AssetPicker({ open, onPick, onClose, themeLogo = false }: Props) {
  const { ws = "" } = useParams();
  return open ? <Picker workspaceId={ws} onPick={onPick} onClose={onClose} themeLogo={themeLogo} /> : null;
}

function Picker({ workspaceId, onPick, onClose, themeLogo }: Omit<Props, "open"> & { workspaceId: string }) {
  const access = useAssetAccess(workspaceId);
  const { resource, hasMore, loadingMore, loadMoreFailed, loadMore, add } = useAssets(workspaceId);
  const maxBytes = resource.status === "ready" ? resource.data.maxFileBytes : DEFAULT_MAX_FILE_BYTES;
  const { sending, failure, upload, cancel } = useAssetUpload(workspaceId, maxBytes);
  const [search, setSearch] = useState("");
  const [choice, setChoice] = useState<string | null>(null);
  const [byAddress, setByAddress] = useState(false);
  const [address, setAddress] = useState("");
  const chooser = useRef<HTMLInputElement>(null);
  const group = useRef<HTMLDivElement>(null);

  async function onChoose(event: ChangeEvent<HTMLInputElement>) {
    const file = event.target.files?.[0];
    event.target.value = "";
    if (!file) return;
    const uploaded = await upload(file);
    if (uploaded) {
      if (uploaded.created) add(uploaded.asset);
      setChoice(uploaded.asset.id);
    }
  }

  function close() {
    cancel();
    onClose();
  }

  function onKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    const forward = event.key === "ArrowRight" || event.key === "ArrowDown";
    if (!forward && event.key !== "ArrowLeft" && event.key !== "ArrowUp") return;
    const radios = Array.from(group.current?.querySelectorAll<HTMLButtonElement>('[role="radio"]') ?? []);
    const at = radios.findIndex((radio) => radio === document.activeElement);
    const target = radios[(at + (forward ? 1 : -1) + radios.length) % radios.length];
    if (!target || radios.length === 0) return;
    event.preventDefault();
    target.focus();
    target.click();
  }

  const assets = resource.status === "ready" ? resource.data.assets : [];
  const picked = assets.find((asset) => asset.id === choice);
  const typed = httpsAddress(address);
  const ready = byAddress ? typed !== null : choice === THEME_LOGO || picked !== undefined;

  function use() {
    if (byAddress) {
      if (typed) onPick({ url: typed });
    } else if (choice === THEME_LOGO) {
      onPick({ themeLogo: true });
    } else if (picked) {
      onPick({ asset: { id: picked.id, url: picked.url } });
    }
  }

  const shown = assets.filter((asset) => matchesSearch(asset, search));
  const visible = [...(themeLogo ? [THEME_LOGO] : []), ...shown.map((asset) => asset.id)];
  const stop = choice !== null && visible.includes(choice) ? choice : visible[0];
  const tabStop = (key: string) => (key === stop ? 0 : -1);

  return (
    <Dialog title="Choose an image" onClose={close} wide>
      <div className="stack">
        {byAddress ? (
          <TextField
            label="Web address"
            type="url"
            value={address}
            onChange={(event) => setAddress(event.target.value)}
            placeholder="https://"
            hint="The image must be at an https address."
            error={address.trim() !== "" && typed === null ? "Use an address that starts with https://" : undefined}
          />
        ) : (
          <>
            <input
              className="input"
              type="search"
              placeholder="Search images"
              aria-label="Search images"
              value={search}
              onChange={(event) => setSearch(event.target.value)}
            />
            {failure ? <InlineError {...failure} /> : null}
            <ResourceView resource={resource} loadingLabel="Loading images">
              {() => (
                <>
                  <ul className="image-grid image-grid-compact" aria-label="Images">
                    {access.canWrite ? (
                      <>
                        <li>
                          <input
                            ref={chooser}
                            className="visually-hidden"
                            type="file"
                            accept={ACCEPTED_TYPES}
                            aria-label="Choose an image file"
                            tabIndex={-1}
                            onChange={(event) => void onChoose(event)}
                          />
                          <button
                            type="button"
                            className="image-tile image-tile-dashed"
                            disabled={sending !== null}
                            onClick={() => chooser.current?.click()}
                          >
                            <span className="image-thumb" aria-hidden="true">
                              <UploadIcon />
                            </span>
                            <span className="image-name">Upload</span>
                            <span className="image-meta">PNG, JPEG, GIF, WebP</span>
                          </button>
                        </li>
                        {sending ? <SendingTile sending={sending} /> : null}
                      </>
                    ) : null}
                    <li className="image-picker-group" role="presentation">
                      <div className="image-picker-group" role="radiogroup" aria-label="Images" ref={group} onKeyDown={onKeyDown}>
                        {themeLogo ? (
                          <div>
                            <button
                              type="button"
                              role="radio"
                              aria-checked={choice === THEME_LOGO}
                              tabIndex={tabStop(THEME_LOGO)}
                              className="image-tile"
                              onClick={() => setChoice(THEME_LOGO)}
                            >
                              <span className="image-thumb" aria-hidden="true" />
                              <span className="image-name">Theme logo</span>
                              <span className="image-meta">Follows the theme</span>
                            </button>
                          </div>
                        ) : null}
                        {shown.map((asset) => (
                            <div key={asset.id}>
                              <button
                                type="button"
                                role="radio"
                                aria-checked={choice === asset.id}
                                tabIndex={tabStop(asset.id)}
                                className="image-tile"
                                onClick={() => setChoice(asset.id)}
                              >
                                <Thumb asset={asset} />
                                <span className="image-name">{asset.name}</span>
                                <span className="image-meta">{dimensionsText(asset)}</span>
                              </button>
                            </div>
                          ))}
                      </div>
                    </li>
                  </ul>
                  {assets.length === 0 ? (
                    <EmptyState title="No images yet">
                      {access.canWrite ? "Upload a PNG, JPEG, GIF or WebP, or drop it here." : "Members of the workspace upload the images."}
                    </EmptyState>
                  ) : shown.length === 0 ? (
                    <p className="muted">No image is named like that.</p>
                  ) : null}
                  {hasMore ? (
                    <div>
                      <Button onClick={loadMore} disabled={loadingMore}>
                        {loadingMore ? "Loading" : "Show more"}
                      </Button>
                      {loadMoreFailed ? <span className="field-error"> Could not load more images. Try again.</span> : null}
                    </div>
                  ) : null}
                </>
              )}
            </ResourceView>
          </>
        )}
        <div className="dialog-actions dialog-actions-wrap">
          <Button onClick={() => setByAddress(!byAddress)} className="btn-ghost">
            {byAddress ? "Choose from images" : "Use a web address"}
          </Button>
          <Button onClick={close}>Cancel</Button>
          <Button variant="primary" disabled={!ready} onClick={use}>
            Use image
          </Button>
        </div>
      </div>
    </Dialog>
  );
}
