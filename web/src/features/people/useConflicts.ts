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
  // Keyed to the first page, like `extra`: a new search, filter or reload makes an old failure vanish.
  const [moreFailure, setMoreFailure] = useState<{ base: object; error: unknown } | null>(null);

  const base = first.status === "ready" ? first.data : null;
  const moreError = moreFailure !== null && moreFailure.base === base ? moreFailure.error : null;
  const more = extra !== null && extra.base === base ? extra : null;
  const conflicts = base ? [...base.conflicts, ...(more?.conflicts ?? [])] : [];
  const nextBefore = base ? (more ? more.nextBefore : (base.next_cursor ?? null)) : null;

  const loadMore = useCallback(() => {
    if (base === null || nextBefore === null) return;
    setLoadingMore(true);
    setMoreFailure(null);
    unwrap(
      client.GET("/api/v1/workspaces/{ws}/identity-conflicts", {
        params: { path: { ws: workspaceId }, query: { status, limit: PAGE_SIZE, cursor: nextBefore } },
      }),
    ).then(
      (page) => {
        setExtra((current) => ({
          base,
          conflicts: [...(current?.base === base ? current.conflicts : []), ...page.conflicts],
          nextBefore: page.next_cursor ?? null,
        }));
        setLoadingMore(false);
      },
      (failure: unknown) => {
        setMoreFailure({ base, error: failure });
        setLoadingMore(false);
      },
    );
  }, [base, nextBefore, workspaceId, status]);

  const resource: Resource<Conflict[]> = first.status === "ready" ? { ...first, data: conflicts } : first;
  return { resource, nextBefore, loadingMore, moreError, loadMore };
}
