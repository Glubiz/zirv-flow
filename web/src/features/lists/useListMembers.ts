import { useCallback, useState } from "react";

import { client, unwrap, useResource, type components, type Resource } from "../../api";

export type ListMember = components["schemas"]["ListMemberBody"];
type Page = components["schemas"]["ListMembersBody"];

const PAGE_SIZE = 50;

interface Extra {
  base: Page;
  members: ListMember[];
  nextBefore: string | null;
}

export interface ListMembersState {
  /** The first page, then every page loaded since. */
  resource: Resource<ListMember[]>;
  nextBefore: string | null;
  loadingMore: boolean;
  moreError: unknown;
  loadMore: () => void;
}

export function useListMembers(workspaceId: string, listId: string): ListMembersState {
  const first = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/lists/{id}/members", {
          params: { path: { ws: workspaceId, id: listId }, query: { limit: PAGE_SIZE } },
        }),
      ),
    [workspaceId, listId],
  );
  const [extra, setExtra] = useState<Extra | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [moreError, setMoreError] = useState<unknown>(null);

  const base = first.status === "ready" ? first.data : null;
  const more = extra !== null && extra.base === base ? extra : null;
  const members = base ? [...base.members, ...(more?.members ?? [])] : [];
  const nextBefore = base ? (more ? more.nextBefore : (base.next_cursor ?? null)) : null;

  const loadMore = useCallback(() => {
    if (base === null || nextBefore === null) return;
    setLoadingMore(true);
    setMoreError(null);
    unwrap(
      client.GET("/api/v1/workspaces/{ws}/lists/{id}/members", {
        params: { path: { ws: workspaceId, id: listId }, query: { limit: PAGE_SIZE, cursor: nextBefore } },
      }),
    ).then(
      (page) => {
        setExtra((current) => ({
          base,
          members: [...(current?.base === base ? current.members : []), ...page.members],
          nextBefore: page.next_cursor ?? null,
        }));
        setLoadingMore(false);
      },
      (failure: unknown) => {
        setMoreError(failure);
        setLoadingMore(false);
      },
    );
  }, [base, nextBefore, workspaceId, listId]);

  const resource: Resource<ListMember[]> = first.status === "ready" ? { ...first, data: members } : first;
  return { resource, nextBefore, loadingMore, moreError, loadMore };
}
