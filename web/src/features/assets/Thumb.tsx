import type { Asset } from "./model";
import type { Sending } from "./useAssetUpload";

/** The thumbnail of an image, loaded from its public address. */
export function Thumb({ asset }: { asset: Pick<Asset, "url"> }) {
  return (
    <span className="image-thumb" aria-hidden="true">
      <img src={asset.url} alt="" loading="lazy" />
    </span>
  );
}

export function UploadIcon() {
  return (
    <svg aria-hidden="true" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round">
      <path d="M8 10.5V2.5M4.5 6L8 2.5 11.5 6M2.5 10.5v3h11v-3" />
    </svg>
  );
}

/** The tile of an upload on its way: the name and how much has gone. */
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
