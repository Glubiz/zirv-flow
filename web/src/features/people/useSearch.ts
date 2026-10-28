import { useCallback, useState } from "react";

import { client, unwrap, useResource, type components, type Resource } from "../../api";

export type PersonHit = components["schemas"]["PersonHitBody"];
type Page = components["schemas"]["SearchBody"];
type SearchRequest = components["schemas"]["SearchRequest"];

const PAGE_SIZE = 50;

/** What was searched for, and when: running the same search again reads it again. */
export interface SearchQuery {
  run: number;
  request: Omit<SearchRequest, "limit" | "cursor">;
}

interface Extra {
  base: Page | null;
  people: PersonHit[];
  nextAfter: string | null;
}

export interface SearchState {
  /** The first page, then every page loaded since. */
  resource: Resource<PersonHit[]>;
  nextAfter: string | null;
  loadingMore: boolean;
  moreError: unknown;
  loadMore: () => void;
}

/** People matching `query`, paged by person id. A null query reads nothing. */
export function useSearch(workspaceId: string, query: SearchQuery | null): SearchState {
  const first = useResource(
    () =>
      query === null
        ? Promise.resolve(null)
        : unwrap(
            client.POST("/api/v1/workspaces/{ws}/people/search", {
              params: { path: { ws: workspaceId } },
              body: { ...query.request, limit: PAGE_SIZE },
            }),
          ),
    [workspaceId, query === null ? "none" : `${query.run}:${JSON.stringify(query.request)}`],
  );
  const [extra, setExtra] = useState<Extra | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [moreError, setMoreError] = useState<unknown>(null);

  const base = first.status === "ready" ? first.data : null;
  const more = extra !== null && extra.base === base ? extra : null;
  const people = base ? [...base.people, ...(more?.people ?? [])] : [];
  const nextAfter = base ? (more ? more.nextAfter : (base.next_cursor ?? null)) : null;

  const loadMore = useCallback(() => {
    if (query === null || base === null || nextAfter === null) return;
    setLoadingMore(true);
    setMoreError(null);
    unwrap(
      client.POST("/api/v1/workspaces/{ws}/people/search", {
        params: { path: { ws: workspaceId } },
        body: { ...query.request, limit: PAGE_SIZE, cursor: nextAfter },
      }),
    ).then(
      (page) => {
        setExtra((current) => ({
          base,
          people: [...(current?.base === base ? current.people : []), ...page.people],
          nextAfter: page.next_cursor ?? null,
        }));
        setLoadingMore(false);
      },
      (failure: unknown) => {
        setMoreError(failure);
        setLoadingMore(false);
      },
    );
  }, [query, base, nextAfter, workspaceId]);

  const resource: Resource<PersonHit[]> = first.status === "ready" ? { ...first, data: people } : first;
  return { resource, nextAfter, loadingMore, moreError, loadMore };
}
