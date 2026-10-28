import { useCallback, useState } from "react";

import { client, unwrap, useResource, type components } from "../../api";

export type DeadJob = components["schemas"]["JobBody"];
type Page = components["schemas"]["DeadJobsBody"];

const PAGE_SIZE = 50;

interface Extra {
  base: Page;
  jobs: DeadJob[];
  nextBefore: string | null;
}

export interface DeadJobsState {
  status: "loading" | "ready" | "error";
  jobs: DeadJob[];
  nextBefore: string | null;
  loadingMore: boolean;
  error: unknown;
  moreError: unknown;
  reload: () => void;
  loadMore: () => void;
}

export function useDeadJobs(): DeadJobsState {
  const first = useResource(
    () => unwrap(client.GET("/api/v1/admin/jobs/dead", { params: { query: { limit: PAGE_SIZE } } })),
    [],
  );
  const [extra, setExtra] = useState<Extra | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [moreError, setMoreError] = useState<unknown>(null);

  const base = first.status === "ready" ? first.data : null;
  const more = extra !== null && extra.base === base ? extra : null;
  const jobs = base ? [...base.jobs, ...(more?.jobs ?? [])] : [];
  const nextBefore = base ? (more ? more.nextBefore : (base.next_cursor ?? null)) : null;

  const loadMore = useCallback(() => {
    if (base === null || nextBefore === null) return;
    setLoadingMore(true);
    setMoreError(null);
    unwrap(client.GET("/api/v1/admin/jobs/dead", { params: { query: { limit: PAGE_SIZE, cursor: nextBefore } } })).then(
      (page) => {
        setExtra((current) => ({
          base,
          jobs: [...(current?.base === base ? current.jobs : []), ...page.jobs],
          nextBefore: page.next_cursor ?? null,
        }));
        setLoadingMore(false);
      },
      (failure: unknown) => {
        setMoreError(failure);
        setLoadingMore(false);
      },
    );
  }, [base, nextBefore]);

  return {
    status: first.status,
    jobs,
    nextBefore,
    loadingMore,
    error: first.status === "error" ? first.error : null,
    moreError,
    reload: first.reload,
    loadMore,
  };
}
