import { useCallback, useState } from "react";

import { client, unwrap, useResource, type Resource } from "../../api";
import type { Asset } from "./model";

const PAGE = 50;

export interface AssetList {
  assets: Asset[];
  usedBytes: number;
  limitBytes: number;
  maxFileBytes: number;
}

interface Later {
  /** The first page these pages follow; a newer first page makes them stale. */
  after: unknown;
  assets: Asset[];
  next: string | null;
}

export interface UseAssets {
  resource: Resource<AssetList>;
  hasMore: boolean;
  loadingMore: boolean;
  loadMoreFailed: boolean;
  loadMore: () => void;
  /** Shows an asset at once, before the list is read again. */
  add: (asset: Asset) => void;
  /** Reads the list again, from the newest. */
  reload: () => void;
  /** Drops an asset that was added in this view, once it is deleted. */
  forget: (id: string) => void;
}

/** The workspace's images, newest first, with the quota; later pages are fetched on request. */
export function useAssets(workspaceId: string): UseAssets {
  const first = useResource(
    () =>
      unwrap(client.GET("/api/v1/workspaces/{ws}/assets", { params: { path: { ws: workspaceId }, query: { limit: PAGE } } })),
    [workspaceId],
  );
  const [later, setLater] = useState<Later | null>(null);
  const [added, setAdded] = useState<Asset[]>([]);
  const [loadingMore, setLoadingMore] = useState(false);
  const [loadMoreFailed, setLoadMoreFailed] = useState(false);

  const firstData = first.status === "ready" ? first.data : null;
  const pages = later && later.after === firstData ? later : null;
  const next = pages ? pages.next : (firstData?.next_before ?? null);

  const loadMore = useCallback(() => {
    if (!next || loadingMore) return;
    setLoadingMore(true);
    setLoadMoreFailed(false);
    unwrap(
      client.GET("/api/v1/workspaces/{ws}/assets", { params: { path: { ws: workspaceId }, query: { limit: PAGE, before: next } } }),
    ).then(
      (page) => {
        setLater((current) => ({
          after: firstData,
          assets: [...(current && current.after === firstData ? current.assets : []), ...page.assets],
          next: page.next_before ?? null,
        }));
        setLoadingMore(false);
      },
      () => {
        setLoadMoreFailed(true);
        setLoadingMore(false);
      },
    );
  }, [next, loadingMore, workspaceId, firstData]);


  const add = useCallback((asset: Asset) => setAdded((current) => [asset, ...current.filter((item) => item.id !== asset.id)]), []);
  const forget = useCallback((id: string) => setAdded((current) => current.filter((item) => item.id !== id)), []);

  const resource: Resource<AssetList> =
    first.status !== "ready"
      ? first
      : {
          ...first,
          data: {
            assets: dedupe([...added, ...first.data.assets, ...(pages?.assets ?? [])]),
            usedBytes: first.data.used_bytes,
            limitBytes: first.data.limit_bytes,
            maxFileBytes: first.data.max_file_bytes,
          },
        };

  return { resource, hasMore: next !== null, loadingMore, loadMoreFailed, loadMore, add, reload: first.reload, forget };
}

function dedupe(assets: Asset[]): Asset[] {
  const seen = new Set<string>();
  return assets.filter((asset) => !seen.has(asset.id) && !!seen.add(asset.id));
}
