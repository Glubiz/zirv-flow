import { useCallback, useState } from "react";

import { client, unwrap, useResource, type components, type Resource } from "../../api";
import type { Message } from "./messageView";

type Page = components["schemas"]["MessagesBody"];

const PAGE_SIZE = 25;

export type ResultFilter = "" | "sent" | "skipped" | "deferred" | "failed";

export interface MessageFilter {
  result: ResultFilter;
  /** A template id, or empty for all. */
  template: string;
}

interface Extra {
  base: Page;
  messages: Message[];
  nextBefore: string | null;
}

export interface MessageLogState {
  /** The newest page, then every older page loaded since. */
  resource: Resource<Message[]>;
  nextBefore: string | null;
  loadingMore: boolean;
  moreError: unknown;
  loadOlder: () => void;
}

function query(filter: MessageFilter, before?: string) {
  return {
    limit: PAGE_SIZE,
    ...(filter.result ? { result: filter.result } : {}),
    ...(filter.template ? { template_id: filter.template } : {}),
    ...(before ? { before } : {}),
  };
}

/** A person's message log, newest first, narrowed by the filter. */
export function useMessageLog(workspaceId: string, personId: string, filter: MessageFilter): MessageLogState {
  const first = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/people/{id}/messages", {
          params: { path: { ws: workspaceId, id: personId }, query: query(filter) },
        }),
      ),
    [workspaceId, personId, filter.result, filter.template],
  );
  const [extra, setExtra] = useState<Extra | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [moreError, setMoreError] = useState<unknown>(null);

  const base = first.status === "ready" ? first.data : null;
  const more = extra !== null && extra.base === base ? extra : null;
  const messages = base ? [...base.messages, ...(more?.messages ?? [])] : [];
  const nextBefore = base ? (more ? more.nextBefore : (base.next_before ?? null)) : null;

  const loadOlder = useCallback(() => {
    if (base === null || nextBefore === null) return;
    setLoadingMore(true);
    setMoreError(null);
    unwrap(
      client.GET("/api/v1/workspaces/{ws}/people/{id}/messages", {
        params: { path: { ws: workspaceId, id: personId }, query: query(filter, nextBefore) },
      }),
    ).then(
      (page) => {
        setExtra((current) => ({
          base,
          messages: [...(current?.base === base ? current.messages : []), ...page.messages],
          nextBefore: page.next_before ?? null,
        }));
        setLoadingMore(false);
      },
      (failure: unknown) => {
        setMoreError(failure);
        setLoadingMore(false);
      },
    );
  }, [base, nextBefore, workspaceId, personId, filter]);

  const resource: Resource<Message[]> = first.status === "ready" ? { ...first, data: messages } : first;
  return { resource, nextBefore, loadingMore, moreError, loadOlder };
}
