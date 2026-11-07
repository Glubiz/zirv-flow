import type { ReactNode } from "react";

import { ApiError, describeError, type Resource } from "../api";
import { EmptyState, ErrorState, LoadingState } from "./States";

interface LoadErrorProps {
  error: unknown;
  onRetry: () => void;
  /** What the page shows, such as "segment": a 404 then reads as a missing record, with no retry. */
  noun?: string;
  action?: ReactNode;
}

/** The failure of a page's main load; a 404 for a named record says it no longer exists instead of offering a retry. */
export function LoadError({ error, onRetry, noun, action }: LoadErrorProps) {
  if (noun && error instanceof ApiError && error.status === 404) {
    return (
      <EmptyState title={`This ${noun} does not exist`} action={action}>
        It may have been deleted, or the address is wrong.
      </EmptyState>
    );
  }
  return <ErrorState {...describeError(error)} onRetry={onRetry} />;
}

interface ResourceViewProps<T> {
  resource: Resource<T>;
  /** Set on a page's main record so that an unknown id reads as missing. */
  noun?: string;
  loadingLabel: string;
  isEmpty?: (data: T) => boolean;
  empty?: ReactNode;
  children: (data: T) => ReactNode;
}

export function ResourceView<T>({ resource, loadingLabel, isEmpty, empty, children, noun }: ResourceViewProps<T>) {
  if (resource.status === "loading") {
    return <LoadingState label={loadingLabel} />;
  }
  if (resource.status === "error") {
    return <LoadError error={resource.error} onRetry={resource.reload} noun={noun} />;
  }
  if (isEmpty?.(resource.data)) {
    return <>{empty ?? <EmptyState title="Nothing here yet" />}</>;
  }
  return <>{children(resource.data)}</>;
}
