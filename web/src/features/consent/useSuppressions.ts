import { useCallback, useEffect, useRef, useState } from "react";

import { client, unwrap, useResource, type components, type Resource } from "../../api";

export type Suppression = components["schemas"]["SuppressionBody"];
type Page = components["schemas"]["SuppressionsBody"];

const PAGE_SIZE = 50;

interface Extra {
  base: Page;
  suppressions: Suppression[];
  nextBefore: string | null;
}

/** Whether the load more of the page `base` is running or failed; one that belongs to an older page is ignored. */
interface Progress {
  base: Page;
  loading: boolean;
  error: unknown;
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
  const [progress, setProgress] = useState<Progress | null>(null);

  const base = first.status === "ready" ? first.data : null;
  const current = useRef(base);
  useEffect(() => {
    current.current = base;
  }, [base]);
  const mine = progress !== null && progress.base === base ? progress : null;
  const loadingMore = mine?.loading ?? false;
  const moreError = mine?.error ?? null;
  const more = extra !== null && extra.base === base ? extra : null;
  const suppressions = base ? [...base.suppressions, ...(more?.suppressions ?? [])] : [];
  const nextBefore = base ? (more ? more.nextBefore : (base.next_cursor ?? null)) : null;

  const loadMore = useCallback(() => {
    if (base === null || nextBefore === null) return;
    setProgress({ base, loading: true, error: null });
    unwrap(
      client.GET("/api/v1/workspaces/{ws}/suppressions", {
        params: { path: { ws: workspaceId }, query: { limit: PAGE_SIZE, cursor: nextBefore, reason: filter } },
      }),
    ).then(
      (page) => {
        if (current.current !== base) return;
        setExtra((current) => ({
          base,
          suppressions: [...(current?.base === base ? current.suppressions : []), ...page.suppressions],
          nextBefore: page.next_cursor ?? null,
        }));
        setProgress({ base, loading: false, error: null });
      },
      (failure: unknown) => {
        if (current.current !== base) return;
        setProgress({ base, loading: false, error: failure });
      },
    );
  }, [base, nextBefore, workspaceId, filter]);

  const resource: Resource<Suppression[]> = first.status === "ready" ? { ...first, data: suppressions } : first;
  return { resource, nextBefore, loadingMore, moreError, loadMore };
}
