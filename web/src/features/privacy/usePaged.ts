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

interface Known<T> {
  inputs: string;
  page: Page<T>;
}

export interface Paged<T> {
  /** The first page, then every page loaded since. */
  resource: Resource<T[]>;
  next: string | null;
  loadingMore: boolean;
  moreError: unknown;
  /** Why the latest refresh of the first page failed; the last page that loaded stays shown. */
  refreshError: unknown;
  loadMore: () => void;
}

/**
 * A keyset-paged list: the first page loads with the resource, later pages on request. Once a
 * first page has loaded it stays on screen while a reload runs or fails, so a poll that fails
 * once does not lose the list and the next beat can try again.
 */
export function usePaged<T>(fetchPage: (before: string | null) => Promise<Page<T>>, inputs: readonly string[]): Paged<T> {
  const first = useResource(() => fetchPage(null), inputs);
  const [extra, setExtra] = useState<Extra<T> | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [moreError, setMoreError] = useState<unknown>(null);
  const [known, setKnown] = useState<Known<T> | null>(null);

  const key = inputs.join("|");
  if (first.status === "ready" && (known?.inputs !== key || known.page !== first.data)) {
    setKnown({ inputs: key, page: first.data });
  }

  const kept = known?.inputs === key ? known.page : null;
  const base = first.status === "ready" ? first.data : kept;
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

  const resource: Resource<T[]> =
    first.status === "ready"
      ? { ...first, data: items }
      : base
        ? { status: "ready", data: items, reload: first.reload, refreshing: first.status === "loading" }
        : first;
  const refreshError = first.status === "error" && base ? first.error : null;
  return { resource, next, loadingMore, moreError, refreshError, loadMore };
}
