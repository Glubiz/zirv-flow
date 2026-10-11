import type { ReactNode } from "react";

import { Button } from "./Button";

export function LoadingState({ label }: { label: string }) {
  return (
    <div className="loading" role="status">
      <span className="spinner" aria-hidden="true" />
      <span>{label}</span>
    </div>
  );
}

interface EmptyStateProps {
  title: string;
  children?: ReactNode;
  action?: ReactNode;
}

export function EmptyState({ title, children, action }: EmptyStateProps) {
  return (
    <div className="state">
      <p className="state-title">{title}</p>
      {children ? <p className="state-body">{children}</p> : null}
      {action ? <div className="state-actions">{action}</div> : null}
    </div>
  );
}

interface ErrorStateProps {
  title: string;
  detail?: string;
  traceId?: string | null;
  onRetry?: () => void;
}

export function ErrorState({ title, detail, traceId, onRetry }: ErrorStateProps) {
  return (
    <div className="state state-error" role="alert">
      <p className="state-title">{title}</p>
      {detail ? <p className="state-body">{detail}</p> : null}
      {traceId ? (
        <p className="state-trace">
          Trace ID <span className="mono">{traceId}</span>
        </p>
      ) : null}
      {onRetry ? (
        <div className="state-actions">
          <Button onClick={onRetry}>Try again</Button>
        </div>
      ) : null}
    </div>
  );
}

interface InlineErrorProps {
  title: string;
  detail?: string;
  traceId?: string | null;
}

export function InlineError({ title, detail, traceId }: InlineErrorProps) {
  return (
    <div className="form-error" role="alert">
      <span className="strong">{title}</span>
      {detail ? <span>{detail}</span> : null}
      {traceId ? (
        <span className="faint">
          {" "}
          Trace ID <span className="mono">{traceId}</span>
        </span>
      ) : null}
    </div>
  );
}
