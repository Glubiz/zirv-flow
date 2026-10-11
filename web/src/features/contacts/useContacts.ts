import { useCallback, useState } from "react";

import { client, unwrap, useResource, type Resource } from "../../api";
import type { Contact } from "./schema";

const PAGE_SIZE = 50;

interface Page {
  contacts: Contact[];
  next_after: string | null;
}

/** What was searched for, and when: running the same search again reads it again. */
export interface ContactQuery {
  run: number;
  /** A key prefix, an email or a phone number; null lists every contact. */
  text: string | null;
}

interface More {
  base: Page;
  contacts: Contact[];
  nextAfter: string | null;
}

export interface ContactList {
  /** The first page, then every page loaded since. */
  resource: Resource<Contact[]>;
  nextAfter: string | null;
  loadingMore: boolean;
  moreError: unknown;
  loadMore: () => void;
  reload: () => void;
}

/**
 * The workspace's contacts in key order, paged by key; or, when `query.text` is set, the
 * contacts that match it. A search is not paged: it returns up to a page of matches.
 */
export function useContacts(workspaceId: string, query: ContactQuery): ContactList {
  const searching = query.text !== null;
  const first = useResource<Page>(
    async () => {
      if (query.text !== null) {
        const found = await unwrap(
          client.POST("/api/v1/workspaces/{ws}/contacts/search", {
            params: { path: { ws: workspaceId } },
            body: { q: query.text, limit: PAGE_SIZE },
          }),
        );
        return { contacts: found.contacts, next_after: null };
      }
      const page = await unwrap(
        client.GET("/api/v1/workspaces/{ws}/contacts", {
          params: { path: { ws: workspaceId }, query: { limit: PAGE_SIZE } },
        }),
      );
      return { contacts: page.contacts, next_after: page.next_after ?? null };
    },
    [workspaceId, searching ? `search:${query.run}:${query.text}` : "list"],
  );
  const [more, setMore] = useState<More | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [moreError, setMoreError] = useState<unknown>(null);

  const base = first.status === "ready" ? first.data : null;
  const extra = more !== null && more.base === base ? more : null;
  const nextAfter = base ? (extra ? extra.nextAfter : base.next_after) : null;

  const loadMore = useCallback(() => {
    if (base === null || nextAfter === null) return;
    setLoadingMore(true);
    setMoreError(null);
    unwrap(
      client.GET("/api/v1/workspaces/{ws}/contacts", {
        params: { path: { ws: workspaceId }, query: { limit: PAGE_SIZE, after: nextAfter } },
      }),
    )
      .then((page) =>
        setMore({
          base,
          contacts: [...(extra?.contacts ?? []), ...page.contacts],
          nextAfter: page.next_after ?? null,
        }),
      )
      .catch((error: unknown) => setMoreError(error))
      .finally(() => setLoadingMore(false));
  }, [base, extra, nextAfter, workspaceId]);

  const resource: Resource<Contact[]> =
    first.status === "ready"
      ? { ...first, data: [...first.data.contacts, ...(extra?.contacts ?? [])] }
      : first;
  return { resource, nextAfter, loadingMore, moreError, loadMore, reload: first.reload };
}
