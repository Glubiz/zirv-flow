import { useId, type ReactNode } from "react";

interface PageHeaderProps {
  title: string;
  lede?: string;
  actions?: ReactNode;
}

export function PageHeader({ title, lede, actions }: PageHeaderProps) {
  return (
    <div className="page-head">
      <div className="page-head-text">
        <h1 className="page-title">{title}</h1>
        {lede ? <p className="page-lede">{lede}</p> : null}
      </div>
      {actions ? <div className="page-actions">{actions}</div> : null}
    </div>
  );
}

interface PanelProps {
  title?: string;
  flush?: boolean;
  children: ReactNode;
}

export function Panel({ title, flush = false, children }: PanelProps) {
  const headingId = useId();
  return (
    <section className={flush ? "panel panel-flush" : "panel panel-pad"} aria-labelledby={title ? headingId : undefined}>
      {title ? (
        <h2 className="panel-title" id={headingId}>
          {title}
        </h2>
      ) : null}
      {children}
    </section>
  );
}
