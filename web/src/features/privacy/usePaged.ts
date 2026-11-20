import { useCallback, useEffect, useRef, useState } from "react";

import { useResource, type Resource } from "../../api";

export interface Page<T> {
  items: T[];
  next: string | null;
}

/** The first page and the pages after it, as one list; `extra` is how many further pages it holds. */
interface Chain<T> extends Page<T> {
  extra: number;
  /** The depth this load asked for; it may reach less when the list got shorter. */
  wanted: number;
}

interface Known<T> {
  inputs: string;
  /** The refresh result this chain came from; a chain built by Show more keeps it so it is not re-synced. */
  source: Chain<T>;
  chain: Chain<T>;
}

export interface Paged<T> {
  /** The first page, then every page loaded since. */
  resource: Resource<T[]>;
  next: string | null;
  loadingMore: boolean;
  moreError: unknown;
  /** Why the latest refresh failed; the last rows that loaded stay shown. */
  refreshError: unknown;
  loadMore: () => void;
}

/**
 * A keyset-paged list: the first page loads with the resource, later pages on request. A refresh
 * (the resource reload a poll makes) re-fetches the whole loaded chain, the first page and the
 * same number of further pages, so every visible row is fresh and a row pushed off the first page
 * shows up in a later one. Once a chain has loaded it stays on screen while a reload runs or
 * fails. Only the newest refresh applies (useResource drops older ones). A Show more page that
 * lands is appended only if its cursor is still the end of the current chain, else dropped.
 */
export function usePaged<T>(
  fetchPage: (before: string | null) => Promise<Page<T>>,
  inputs: readonly string[],
  idOf: (item: T) => string,
): Paged<T> {
  const key = inputs.join("|");
  const depth = useRef<{ inputs: string; extra: number }>({ inputs: key, extra: 0 });
  const first = useResource<Chain<T>>(async () => {
    // Other inputs start from page one, whatever depth an earlier input reached.
    if (depth.current.inputs !== key) depth.current = { inputs: key, extra: 0 };
    const wanted = depth.current.extra;
    let page = await fetchPage(null);
    let items = page.items;
    let extra = 0;
    while (extra < wanted && page.next !== null) {
      page = await fetchPage(page.next);
      items = [...items, ...page.items];
      extra += 1;
    }
    return { items: uniqueById(items, idOf), next: page.next, extra, wanted };
  }, inputs);
  // The depth wanted now, as state for render; `depth` is the same value for the loader. Other inputs start at 0.
  const [wantedDepth, setWantedDepth] = useState({ inputs: key, extra: 0 });
  if (wantedDepth.inputs !== key) setWantedDepth({ inputs: key, extra: 0 });
  const [loadingMore, setLoadingMore] = useState(false);
  const [moreError, setMoreError] = useState<unknown>(null);
  const [known, setKnown] = useState<Known<T> | null>(null);
  const latest = useRef(known);
  useEffect(() => {
    latest.current = known;
  });

  // A refresh that started before a Show more page landed is shallower than the depth now wanted for these inputs: keep the deeper chain.
  // It is compared with the wanted depth, not the shown chain, so a reload that asks for it cannot be stale again.
  const wantedNow = wantedDepth.inputs === key ? wantedDepth.extra : 0;
  const stale =
    first.status === "ready" && known?.inputs === key && known.source !== first.data && first.data.wanted < wantedNow;
  const reload = first.status === "ready" ? first.reload : null;
  const refreshed = first.status === "ready" ? first.data : null;
  useEffect(() => {
    if (stale && reload && refreshed) reload();
  }, [stale, reload, refreshed]);

  if (first.status === "ready" && !stale && (known?.inputs !== key || known.source !== first.data)) {
    setKnown({ inputs: key, source: first.data, chain: first.data });
  }

  const kept = known?.inputs === key ? known.chain : null;
  const base = first.status === "ready" && known?.source === first.data ? known.chain : kept;
  const shown = base ?? (first.status === "ready" ? first.data : null);
  const items = shown ? shown.items : [];
  const next = shown ? shown.next : null;

  const loadMore = useCallback(() => {
    if (shown === null || next === null) return;
    const cursor = next;
    setLoadingMore(true);
    setMoreError(null);
    fetchPage(cursor).then(
      (page) => {
        const current = latest.current;
        if (current?.inputs === key && current.chain.next === cursor) {
          const chain = {
            items: uniqueById([...current.chain.items, ...page.items], idOf),
            next: page.next,
            extra: current.chain.extra + 1,
            wanted: current.chain.extra + 1,
          };
          depth.current = { inputs: key, extra: chain.extra };
          setWantedDepth(depth.current);
          const appended = { ...current, chain };
          latest.current = appended;
          setKnown(appended);
        }
        setLoadingMore(false);
      },
      (failure: unknown) => {
        setMoreError(failure);
        setLoadingMore(false);
      },
    );
  }, [shown, next, fetchPage, idOf, key]);

  const resource: Resource<T[]> = shown
    ? first.status === "ready"
      ? { ...first, data: items }
      : { status: "ready", data: items, reload: first.reload, refreshing: first.status === "loading" }
    : first.status === "ready"
      ? { ...first, data: [] }
      : first;
  const refreshError = first.status === "error" && shown ? first.error : null;
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
