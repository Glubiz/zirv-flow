import { useCallback, useState } from "react";

import { client, unwrap, useResource, type components, type Resource } from "../../api";

export type Conflict = components["schemas"]["ConflictBody"];
export type ConflictStatus = "open" | "resolved";
type Page = components["schemas"]["ConflictsBody"];

const PAGE_SIZE = 50;

interface Extra {
  base: Page;
  conflicts: Conflict[];
  nextBefore: string | null;
}

export interface ConflictsState {
  /** The first page, then every page loaded since. */
  resource: Resource<Conflict[]>;
  nextBefore: string | null;
  loadingMore: boolean;
  moreError: unknown;
  loadMore: () => void;
}

export function useConflicts(workspaceId: string, status: ConflictStatus): ConflictsState {
  const first = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/identity-conflicts", {
          params: { path: { ws: workspaceId }, query: { status, limit: PAGE_SIZE } },
        }),
      ),
    [workspaceId, status],
  );
  const [extra, setExtra] = useState<Extra | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [moreError, setMoreError] = useState<unknown>(null);

  const base = first.status === "ready" ? first.data : null;
  const more = extra !== null && extra.base === base ? extra : null;
  const conflicts = base ? [...base.conflicts, ...(more?.conflicts ?? [])] : [];
  const nextBefore = base ? (more ? more.nextBefore : (base.next_before ?? null)) : null;

  const loadMore = useCallback(() => {
    if (base === null || nextBefore === null) return;
    setLoadingMore(true);
    setMoreError(null);
    unwrap(
      client.GET("/api/v1/workspaces/{ws}/identity-conflicts", {
        params: { path: { ws: workspaceId }, query: { status, limit: PAGE_SIZE, before: nextBefore } },
      }),
    ).then(
      (page) => {
        setExtra((current) => ({
          base,
          conflicts: [...(current?.base === base ? current.conflicts : []), ...page.conflicts],
          nextBefore: page.next_before ?? null,
        }));
        setLoadingMore(false);
      },
      (failure: unknown) => {
        setMoreError(failure);
        setLoadingMore(false);
      },
    );
  }, [base, nextBefore, workspaceId, status]);

  const resource: Resource<Conflict[]> = first.status === "ready" ? { ...first, data: conflicts } : first;
  return { resource, nextBefore, loadingMore, moreError, loadMore };
}
