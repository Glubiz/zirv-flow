import { ApiError, type components, type ErrorSummary } from "../../api";

export type Asset = components["schemas"]["AssetBody"];
export type AssetDetail = components["schemas"]["AssetDetailBody"];
export type AssetOwner = components["schemas"]["AssetOwnerBody"];

/** The image types the server accepts, for the file chooser. */
export const ACCEPTED_TYPES = "image/png,image/jpeg,image/gif,image/webp";

export const IN_USE_LINE = "Published versions use this image, so it can't be deleted.";

const UNITS = ["B", "KB", "MB", "GB", "TB"] as const;

/** A byte count in the largest fitting unit: "38 KB", "1.5 MB", "1 GB". */
export function formatBytes(bytes: number): string {
  let value = Math.max(0, bytes);
  let unit = 0;
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const text = value >= 10 || Number.isInteger(value) ? String(Math.round(value)) : value.toFixed(1);
  return `${text} ${UNITS[unit]}`;
}

export function dimensionsText(asset: Pick<Asset, "width" | "height">): string {
  return `${asset.width} × ${asset.height}`;
}

export function typeLabel(contentType: string): string {
  const subtype = contentType.split("/")[1] ?? contentType;
  return subtype === "webp" ? "WebP" : subtype.toUpperCase();
}

export function shortHash(sha256: string): string {
  return sha256.length > 13 ? `${sha256.slice(0, 8)}…${sha256.slice(-4)}` : sha256;
}

/** The link to share: a path the server gives when it has no public address is made absolute. */
export function absoluteUrl(url: string): string {
  return new URL(url, globalThis.location.origin).href;
}

export function matchesSearch(asset: Pick<Asset, "name">, search: string): boolean {
  return asset.name.toLowerCase().includes(search.trim().toLowerCase());
}

/** The page of the template, theme or snippet that holds a published version using an image. */
export function ownerPath(workspaceId: string, owner: AssetOwner): string {
  return `/w/${workspaceId}/${owner.kind}s/${owner.id}`;
}

export function ownerKindLabel(kind: string): string {
  return kind.charAt(0).toUpperCase() + kind.slice(1);
}

const MESSAGES: Record<string, string> = {
  asset_type_unsupported: "Only PNG, JPEG, GIF and WebP images can be uploaded.",
  asset_type_mismatch: "The file is not the type it says it is. Export it again as a PNG, JPEG, GIF or WebP.",
  asset_quota_exceeded: "Your image storage is full. Delete images you no longer use, then try again.",
  asset_dimensions_exceeded: "That image is too large in pixels. The most is 8192 pixels on a side and 40 megapixels.",
};

export function tooLargeMessage(maxBytes: number): string {
  return `That image is over ${formatBytes(maxBytes)}. Use a smaller one.`;
}

/** What to tell the person when an upload did not go through: one short message per server code. */
export function uploadFailure(error: unknown, maxBytes: number): ErrorSummary {
  if (error instanceof ApiError) {
    const code = error.code ?? "";
    const message = code === "asset_too_large" ? tooLargeMessage(maxBytes) : MESSAGES[code];
    if (message) return { title: message, detail: "", traceId: null };
    return { title: error.title, detail: error.detail, traceId: error.traceId };
  }
  return {
    title: "Could not reach the server",
    detail: error instanceof Error ? error.message : "Check your connection and try again.",
    traceId: null,
  };
}
