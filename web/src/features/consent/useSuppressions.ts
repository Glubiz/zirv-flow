import { useCallback, useState } from "react";

import { client, unwrap, useResource, type components, type Resource } from "../../api";

export type Suppression = components["schemas"]["SuppressionBody"];
type Page = components["schemas"]["SuppressionsBody"];

const PAGE_SIZE = 50;

interface Extra {
  base: Page;
  suppressions: Suppression[];
  nextBefore: string | null;
}

export interface SuppressionsState {
  /** The first page, then every page loaded since. */
  resource: Resource<Suppression[]>;
  nextBefore: string | null;
  loadingMore: boolean;
  moreError: unknown;
  loadMore: () => void;
}

/** Suppressions newest first, optionally of one reason (an empty reason means all). */
export function useSuppressions(workspaceId: string, reason: string): SuppressionsState {
  const filter = reason === "" ? undefined : reason;
  const first = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/suppressions", {
          params: { path: { ws: workspaceId }, query: { limit: PAGE_SIZE, reason: filter } },
        }),
      ),
    [workspaceId, reason],
  );
  const [extra, setExtra] = useState<Extra | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [moreError, setMoreError] = useState<unknown>(null);

  const base = first.status === "ready" ? first.data : null;
  const more = extra !== null && extra.base === base ? extra : null;
  const suppressions = base ? [...base.suppressions, ...(more?.suppressions ?? [])] : [];
  const nextBefore = base ? (more ? more.nextBefore : (base.next_before ?? null)) : null;

  const loadMore = useCallback(() => {
    if (base === null || nextBefore === null) return;
    setLoadingMore(true);
    setMoreError(null);
    unwrap(
      client.GET("/api/v1/workspaces/{ws}/suppressions", {
        params: { path: { ws: workspaceId }, query: { limit: PAGE_SIZE, before: nextBefore, reason: filter } },
      }),
    ).then(
      (page) => {
        setExtra((current) => ({
          base,
          suppressions: [...(current?.base === base ? current.suppressions : []), ...page.suppressions],
          nextBefore: page.next_before ?? null,
        }));
        setLoadingMore(false);
      },
      (failure: unknown) => {
        setMoreError(failure);
        setLoadingMore(false);
      },
    );
  }, [base, nextBefore, workspaceId, filter]);

  const resource: Resource<Suppression[]> = first.status === "ready" ? { ...first, data: suppressions } : first;
  return { resource, nextBefore, loadingMore, moreError, loadMore };
}
