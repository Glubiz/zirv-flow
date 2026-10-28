import { useCallback, useState } from "react";

import { client, unwrap, useResource, type components, type Resource } from "../../api";

export type TimelineEntry = components["schemas"]["TimelineEntryBody"];
type Page = components["schemas"]["TimelineBody"];

const PAGE_SIZE = 25;

export interface TimelineKinds {
  events: boolean;
  decisions: boolean;
}

interface Extra {
  base: Page;
  entries: TimelineEntry[];
  nextBefore: string | null;
}

export interface TimelineState {
  /** The newest page, then every older page loaded since. */
  resource: Resource<TimelineEntry[]>;
  nextBefore: string | null;
  loadingMore: boolean;
  moreError: unknown;
  loadOlder: () => void;
}

function kindsParam(kinds: TimelineKinds): string {
  return [kinds.events ? "event" : null, kinds.decisions ? "decision" : null].filter(Boolean).join(",");
}

/** A person's timeline, newest first, of the kinds asked for (at least one). */
export function useTimeline(workspaceId: string, personId: string, kinds: TimelineKinds): TimelineState {
  const filter = kindsParam(kinds);
  const first = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/people/{id}/timeline", {
          params: { path: { ws: workspaceId, id: personId }, query: { limit: PAGE_SIZE, kinds: filter } },
        }),
      ),
    [workspaceId, personId, filter],
  );
  const [extra, setExtra] = useState<Extra | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [moreError, setMoreError] = useState<unknown>(null);

  const base = first.status === "ready" ? first.data : null;
  const more = extra !== null && extra.base === base ? extra : null;
  const entries = base ? [...base.entries, ...(more?.entries ?? [])] : [];
  const nextBefore = base ? (more ? more.nextBefore : (base.next_cursor ?? null)) : null;

  const loadOlder = useCallback(() => {
    if (base === null || nextBefore === null) return;
    setLoadingMore(true);
    setMoreError(null);
    unwrap(
      client.GET("/api/v1/workspaces/{ws}/people/{id}/timeline", {
        params: {
          path: { ws: workspaceId, id: personId },
          query: { limit: PAGE_SIZE, kinds: filter, cursor: nextBefore },
        },
      }),
    ).then(
      (page) => {
        setExtra((current) => ({
          base,
          entries: [...(current?.base === base ? current.entries : []), ...page.entries],
          nextBefore: page.next_cursor ?? null,
        }));
        setLoadingMore(false);
      },
      (failure: unknown) => {
        setMoreError(failure);
        setLoadingMore(false);
      },
    );
  }, [base, nextBefore, workspaceId, personId, filter]);

  const resource: Resource<TimelineEntry[]> = first.status === "ready" ? { ...first, data: entries } : first;
  return { resource, nextBefore, loadingMore, moreError, loadOlder };
}
