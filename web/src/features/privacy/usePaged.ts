import { useCallback, useState } from "react";

import { useResource, type Resource } from "../../api";

export interface Page<T> {
  items: T[];
  next: string | null;
}

interface Extra<T> {
  base: Page<T>;
  items: T[];
  next: string | null;
}

export interface Paged<T> {
  /** The first page, then every page loaded since. */
  resource: Resource<T[]>;
  next: string | null;
  loadingMore: boolean;
  moreError: unknown;
  loadMore: () => void;
}

/** A keyset-paged list: the first page loads with the resource, later pages on request. */
export function usePaged<T>(fetchPage: (before: string | null) => Promise<Page<T>>, inputs: readonly string[]): Paged<T> {
  const first = useResource(() => fetchPage(null), inputs);
  const [extra, setExtra] = useState<Extra<T> | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [moreError, setMoreError] = useState<unknown>(null);

  const base = first.status === "ready" ? first.data : null;
  const more = extra !== null && extra.base === base ? extra : null;
  const items = base ? [...base.items, ...(more?.items ?? [])] : [];
  const next = base ? (more ? more.next : base.next) : null;

  const loadMore = useCallback(() => {
    if (base === null || next === null) return;
    setLoadingMore(true);
    setMoreError(null);
    fetchPage(next).then(
      (page) => {
        setExtra((current) => ({
          base,
          items: [...(current?.base === base ? current.items : []), ...page.items],
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

  const resource: Resource<T[]> = first.status === "ready" ? { ...first, data: items } : first;
  return { resource, next, loadingMore, moreError, loadMore };
}
