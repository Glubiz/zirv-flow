import { useCallback, useState } from "react";

import { useResource, type Resource } from "../../api";

export interface Page<T> {
  items: T[];
  next: string | null;
}

interface Extra<T> {
  /** The inputs the pages were loaded for. A poll builds a new first page, so it must not key them. */
  key: string;
  items: T[];
  next: string | null;
}

interface Known<T> {
  inputs: string;
  page: Page<T>;
  /** First-page rows that a refresh pushed past the first page, kept so the loaded pages stay contiguous. */
  tail: T[];
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
export function usePaged<T>(
  fetchPage: (before: string | null) => Promise<Page<T>>,
  inputs: readonly string[],
  idOf: (item: T) => string,
): Paged<T> {
  const first = useResource(() => fetchPage(null), inputs);
  const [extra, setExtra] = useState<Extra<T> | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [moreError, setMoreError] = useState<unknown>(null);
  const [known, setKnown] = useState<Known<T> | null>(null);

  const key = inputs.join("|");
  if (first.status === "ready" && (known?.inputs !== key || known.page !== first.data)) {
    // Rows a refresh pushed out of the first page sit before the loaded pages: keep them, newest first.
    let tail: T[] = [];
    if (known?.inputs === key && extra?.key === key) {
      const present = new Set(first.data.items.map(idOf));
      tail = [...known.page.items, ...known.tail].filter((item) => !present.has(idOf(item)));
    }
    setKnown({ inputs: key, page: first.data, tail });
  }

  const kept = known?.inputs === key ? known.page : null;
  const base = first.status === "ready" ? first.data : kept;
  const more = extra?.key === key ? extra : null;
  const items = base ? uniqueById([...base.items, ...(known?.inputs === key && more ? known.tail : []), ...(more?.items ?? [])], idOf) : [];
  const next = base ? (more ? more.next : base.next) : null;

  const loadMore = useCallback(() => {
    if (base === null || next === null) return;
    setLoadingMore(true);
    setMoreError(null);
    fetchPage(next).then(
      (page) => {
        setExtra((current) => ({
          key,
          items: [...(current?.key === key ? current.items : []), ...page.items],
          next: page.next,
        }));
        setLoadingMore(false);
      },
      (failure: unknown) => {
        setMoreError(failure);
        setLoadingMore(false);
      },
    );
  }, [base, next, fetchPage, key]);

  const resource: Resource<T[]> =
    first.status === "ready"
      ? { ...first, data: items }
      : base
        ? { status: "ready", data: items, reload: first.reload, refreshing: first.status === "loading" }
        : first;
  const refreshError = first.status === "error" && base ? first.error : null;
  return { resource, next, loadingMore, moreError, refreshError, loadMore };
}

function uniqueById<T>(items: T[], idOf: (item: T) => string): T[] {
  const seen = new Set<string>();
  return items.filter((item) => {
    const id = idOf(item);
    if (seen.has(id)) return false;
    seen.add(id);
    return true;
  });
}
