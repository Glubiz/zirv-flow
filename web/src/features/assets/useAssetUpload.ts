import { useEffect, useRef, useState } from "react";

import { uploadFile, type ErrorSummary } from "../../api";
import { tooLargeMessage, uploadFailure, type Asset } from "./model";

const MAX_NAME_CHARS = 200;

const TYPES_BY_EXTENSION: Record<string, string> = {
  png: "image/png",
  jpg: "image/jpeg",
  jpeg: "image/jpeg",
  gif: "image/gif",
  webp: "image/webp",
};

/** What the browser says the file is; when it says nothing, a guess from the extension. The server still decides. */
export function declaredType(file: Pick<File, "name" | "type">): string {
  if (file.type) return file.type;
  const extension = file.name.split(".").pop()?.toLowerCase() ?? "";
  return TYPES_BY_EXTENSION[extension] ?? "application/octet-stream";
}

export interface Sending {
  name: string;
  sent: number;
  total: number;
}

export interface Uploaded {
  asset: Asset;
  created: boolean;
}

export interface AssetUpload {
  sending: Sending | null;
  failure: ErrorSummary | null;
  /**
   * Sends the file; resolves with the asset (`created` is false when the workspace already had the same
   * bytes), or null on a refusal or a cancel.
   */
  upload: (file: File) => Promise<Uploaded | null>;
  /** Stops an upload in flight; its answer is ignored. */
  cancel: () => void;
}

/** Uploads one image at a time: refuses an oversize file before any request, then sends it raw with progress. */
export function useAssetUpload(workspaceId: string, maxBytes: number): AssetUpload {
  const [sending, setSending] = useState<Sending | null>(null);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const controller = useRef<AbortController | null>(null);

  function cancel() {
    controller.current?.abort();
  }

  useEffect(() => () => controller.current?.abort(), []);

  async function upload(file: File): Promise<Uploaded | null> {
    if (sending) return null;
    setFailure(null);
    if (file.size > maxBytes) {
      setFailure({ title: tooLargeMessage(maxBytes), detail: "", traceId: null });
      return null;
    }
    const name = file.name.trim().slice(0, MAX_NAME_CHARS) || "Image";
    setSending({ name, sent: 0, total: file.size });
    const own = new AbortController();
    controller.current = own;
    let created = true;
    try {
      const asset = await uploadFile<Asset>(
        `/api/v1/workspaces/${workspaceId}/assets?${new URLSearchParams({ name }).toString()}`,
        file,
        declaredType(file),
        (sent, total) => setSending({ name, sent, total }),
        { signal: own.signal, onStatus: (status) => (created = status === 201) },
      );
      return own.signal.aborted ? null : { asset, created };
    } catch (error) {
      if (!own.signal.aborted) setFailure(uploadFailure(error, maxBytes));
      return null;
    } finally {
      setSending(null);
    }
  }

  return { sending, failure, upload, cancel };
}
