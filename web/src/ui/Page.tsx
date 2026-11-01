import { useId, type ReactNode } from "react";
import { Link } from "react-router";

export interface Breadcrumb {
  group: string;
  section: string;
  /** Where the section lives; set on detail pages, left out on the list page itself. */
  to?: string;
}

/** "Group / Section": the group is plain text, the section a link on a detail page. */
export function Crumbs({ group, section, to }: Breadcrumb) {
  return (
    <nav aria-label="Breadcrumb" className="crumbs">
      <span>{group}</span>
      <span aria-hidden="true">/</span>
      {to ? <Link to={to}>{section}</Link> : <span>{section}</span>}
    </nav>
  );
}

interface PageHeaderProps {
  title: string;
  lede?: string;
  breadcrumb?: Breadcrumb;
  /** A short count or state beside the title, such as "12,940 people". */
  status?: string;
  primary?: ReactNode;
  secondary?: ReactNode;
  actions?: ReactNode;
}

export function PageHeader({ title, lede, breadcrumb, status, primary, secondary, actions }: PageHeaderProps) {
  const hasActions = Boolean(primary || secondary || actions);
  return (
    <div className="page-head">
      <div className="page-head-text">
        {breadcrumb ? <Crumbs {...breadcrumb} /> : null}
        <div className="page-title-row">
          <h1 className="page-title">{title}</h1>
          {status ? <span className="page-status">{status}</span> : null}
        </div>
        {lede ? <p className="page-lede">{lede}</p> : null}
      </div>
      {hasActions ? (
        <div className="page-actions">
          {secondary}
          {actions}
          {primary}
        </div>
      ) : null}
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

export interface RelatedGroup {
  label: string;
  items: { key: string; label: string; to?: string; meta?: string }[];
}

/** The things a page's subject is used by or built from; groups with no items are not drawn. */
export function RelatedPanel({ groups }: { groups: RelatedGroup[] }) {
  const shown = groups.filter((group) => group.items.length > 0);
  if (shown.length === 0) return null;
  return (
    <aside id="related" aria-label="Related" className="related">
      <Panel title="Related">
        {shown.map((group) => (
          <div key={group.label} className="related-group">
            <h3 className="related-label">{group.label}</h3>
            <ul>
              {group.items.map((item) => (
                <li key={item.key}>
                  {item.to ? <Link to={item.to}>{item.label}</Link> : <span>{item.label}</span>}
                  {item.meta ? <span className="related-meta">{item.meta}</span> : null}
                </li>
              ))}
            </ul>
          </div>
        ))}
      </Panel>
    </aside>
  );
}
