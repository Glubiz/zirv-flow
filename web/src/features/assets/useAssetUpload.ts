import { useState } from "react";

import { uploadFile, type ErrorSummary } from "../../api";
import { tooLargeMessage, uploadFailure, type Asset } from "./model";

const MAX_NAME_CHARS = 200;

export interface Sending {
  name: string;
  sent: number;
  total: number;
}

export interface AssetUpload {
  sending: Sending | null;
  failure: ErrorSummary | null;
  /** Sends the file; resolves with the asset (new, or the existing one for the same bytes), or null on a refusal. */
  upload: (file: File) => Promise<Asset | null>;
}

/** Uploads one image at a time: refuses an oversize file before any request, then sends it raw with progress. */
export function useAssetUpload(workspaceId: string, maxBytes: number): AssetUpload {
  const [sending, setSending] = useState<Sending | null>(null);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function upload(file: File): Promise<Asset | null> {
    if (sending) return null;
    setFailure(null);
    if (file.size > maxBytes) {
      setFailure({ title: tooLargeMessage(maxBytes), detail: "", traceId: null });
      return null;
    }
    const name = file.name.trim().slice(0, MAX_NAME_CHARS) || "Image";
    setSending({ name, sent: 0, total: file.size });
    try {
      return await uploadFile<Asset>(
        `/api/v1/workspaces/${workspaceId}/assets?${new URLSearchParams({ name }).toString()}`,
        file,
        file.type || "application/octet-stream",
        (sent, total) => setSending({ name, sent, total }),
      );
    } catch (error) {
      setFailure(uploadFailure(error, maxBytes));
      return null;
    } finally {
      setSending(null);
    }
  }

  return { sending, failure, upload };
}
