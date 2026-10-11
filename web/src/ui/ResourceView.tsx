import type { ReactNode } from "react";

import { describeError, type Resource } from "../api";
import { EmptyState, ErrorState, LoadingState } from "./States";

interface ResourceViewProps<T> {
  resource: Resource<T>;
  loadingLabel: string;
  isEmpty?: (data: T) => boolean;
  empty?: ReactNode;
  children: (data: T) => ReactNode;
}

export function ResourceView<T>({ resource, loadingLabel, isEmpty, empty, children }: ResourceViewProps<T>) {
  if (resource.status === "loading") {
    return <LoadingState label={loadingLabel} />;
  }
  if (resource.status === "error") {
    return <ErrorState {...describeError(resource.error)} onRetry={resource.reload} />;
  }
  if (isEmpty?.(resource.data)) {
    return <>{empty ?? <EmptyState title="Nothing here yet" />}</>;
  }
  return <>{children(resource.data)}</>;
}
