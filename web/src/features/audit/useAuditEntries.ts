import { useCallback, useState } from "react";

import { client, unwrap, useResource, type Resource } from "../../api";
import { queryFromFilters, type AuditEntry, type Filters } from "./model";

interface Page {
  entries: AuditEntry[];
  next: string | null;
}

interface Extra {
  base: Page;
  entries: AuditEntry[];
  next: string | null;
}

export interface AuditEntries {
  resource: Resource<AuditEntry[]>;
  next: string | null;
  loadingMore: boolean;
  moreError: unknown;
  loadMore: () => void;
}

/** One page of the audit log, newest first, by keyset cursor; the filters or the scope changing starts again at the first page. */
export function useAuditEntries(workspaceId: string, instance: boolean, filters: Filters): AuditEntries {
  const query = queryFromFilters(filters);
  const queryKey = JSON.stringify(query);
  const fetchPage = useCallback(
    async (cursor: string | null): Promise<Page> => {
      const withCursor = { ...query, ...(cursor ? { cursor } : {}) };
      const body = await unwrap(
        instance
          ? client.GET("/api/v1/audit", { params: { query: withCursor } })
          : client.GET("/api/v1/workspaces/{ws}/audit", { params: { path: { ws: workspaceId }, query: withCursor } }),
      );
      return { entries: body.entries, next: body.next_cursor ?? null };
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [workspaceId, instance, queryKey],
  );

  const first = useResource(() => fetchPage(null), [workspaceId, instance, queryKey]);
  const [extra, setExtra] = useState<Extra | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [moreError, setMoreError] = useState<unknown>(null);

  const base = first.status === "ready" ? first.data : null;
  const more = extra !== null && extra.base === base ? extra : null;
  const next = base ? (more ? more.next : base.next) : null;

  const loadMore = useCallback(() => {
    if (base === null || next === null) return;
    setLoadingMore(true);
    setMoreError(null);
    fetchPage(next).then(
      (page) => {
        setExtra((current) => ({
          base,
          entries: [...(current?.base === base ? current.entries : []), ...page.entries],
          next: page.next,
        }));
        setLoadingMore(false);
      },
      (failure: unknown) => {
        setMoreError(failure);
        setLoadingMore(false);
      },
    );
  }, [base, next, fetchPage]);

  const resource: Resource<AuditEntry[]> =
    first.status === "ready" ? { ...first, data: [...first.data.entries, ...(more?.entries ?? [])] } : first;
  return { resource, next, loadingMore, moreError, loadMore };
}

export interface Catalogue {
  areas: { area: string; actions: string[] }[];
}

export function useAuditCatalogue(workspaceId: string): Resource<Catalogue> {
  return useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/audit/actions", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
}
