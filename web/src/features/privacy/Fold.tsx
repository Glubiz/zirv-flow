import type { ReactNode } from "react";

/** A row of the Privacy page that opens to its section. */
export function Fold({ title, children }: { title: string; children: ReactNode }) {
  return (
    <details className="panel fold">
      <summary className="fold-summary">
        <h2 className="panel-title">{title}</h2>
        <span className="fold-chevron" aria-hidden="true" />
      </summary>
      <div className="fold-body stack">{children}</div>
    </details>
  );
}
