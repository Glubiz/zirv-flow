import { useCallback, useState } from "react";

import { useResource, type Resource } from "../../api";

interface Page {
  items: unknown[];
  next_cursor?: string | null;
}

interface More<P extends Page> {
  base: P;
  items: P["items"];
  next: string | null;
}

export interface Paged<P extends Page> {
  /** The first page, with every page loaded since folded into `items`. */
  resource: Resource<{ first: P; items: P["items"] }>;
  hasMore: boolean;
  loadingMore: boolean;
  moreError: unknown;
  loadMore: () => void;
}

/** A list read page by page with a cursor: the first page loads at once, the rest on request. */
export function usePaged<P extends Page>(
  loadPage: (cursor: string | undefined) => Promise<P>,
  inputs: readonly (string | number | boolean | null)[],
): Paged<P> {
  const first = useResource(() => loadPage(undefined), inputs);
  const [extra, setExtra] = useState<More<P> | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [moreError, setMoreError] = useState<unknown>(null);

  const base = first.status === "ready" ? first.data : null;
  const more = extra !== null && extra.base === base ? extra : null;
  const next = base ? (more ? more.next : (base.next_cursor ?? null)) : null;

  const loadMore = useCallback(() => {
    if (base === null || next === null) return;
    setLoadingMore(true);
    setMoreError(null);
    loadPage(next).then(
      (page) => {
        setExtra((current) => ({
          base,
          items: [...(current?.base === base ? current.items : []), ...page.items],
          next: page.next_cursor ?? null,
        }));
        setLoadingMore(false);
      },
      (failure: unknown) => {
        setMoreError(failure);
        setLoadingMore(false);
      },
    );
  }, [base, next, loadPage]);

  const resource: Paged<P>["resource"] =
    first.status === "ready"
      ? { ...first, data: { first: first.data, items: [...first.data.items, ...(more?.items ?? [])] } }
      : first;
  return { resource, hasMore: next !== null, loadingMore, moreError, loadMore };
}
