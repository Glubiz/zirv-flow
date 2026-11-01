import { useRef, type KeyboardEvent } from "react";
import { Link, useSearchParams } from "react-router";

export interface TabEntry {
  id: string;
  label: string;
  count?: number;
}

interface TabState {
  /** The active tab; a missing or unknown value is the first tab. */
  active: string;
  hrefOf: (id: string) => string;
}

/** The tab named by a URL parameter. The first tab is the default and is left out of the URL. */
export function useTabParam(tabs: readonly TabEntry[], param: "tab" | "filter" = "tab"): TabState {
  const [params] = useSearchParams();
  const first = tabs[0]?.id ?? "";
  const asked = params.get(param);
  const active = tabs.some((entry) => entry.id === asked) ? (asked as string) : first;
  const hrefOf = (id: string) => {
    const next = new URLSearchParams(params);
    if (id === first) next.delete(param);
    else next.set(param, id);
    const query = next.toString();
    return query ? `?${query}` : "?";
  };
  return { active, hrefOf };
}

interface TabNavProps {
  label: string;
  tabs: readonly TabEntry[];
  param?: "tab" | "filter";
}

/** Page sections (`?tab=`) and list filters (`?filter=`): links that keep the choice in the URL. */
export function TabNav({ label, tabs, param = "tab" }: TabNavProps) {
  const { active, hrefOf } = useTabParam(tabs, param);
  const nav = useRef<HTMLElement>(null);

  function onKey(event: KeyboardEvent<HTMLElement>) {
    const links = Array.from(nav.current?.querySelectorAll<HTMLAnchorElement>("a") ?? []);
    const at = links.findIndex((link) => link === document.activeElement);
    if (at < 0) return;
    const target =
      event.key === "ArrowRight" ? at + 1 : event.key === "ArrowLeft" ? at - 1 : event.key === "Home" ? 0 : event.key === "End" ? links.length - 1 : null;
    if (target === null) return;
    event.preventDefault();
    links[(target + links.length) % links.length]?.focus();
  }

  return (
    <nav ref={nav} aria-label={label} className="tabnav" onKeyDown={onKey}>
      {tabs.map((entry) => (
        <Link
          key={entry.id}
          to={hrefOf(entry.id)}
          aria-current={entry.id === active ? "page" : undefined}
          tabIndex={entry.id === active ? 0 : -1}
        >
          {entry.label}{entry.count !== undefined ? " " : null}
          {entry.count !== undefined ? <span className="tabnav-count">{entry.count}</span> : null}
        </Link>
      ))}
    </nav>
  );
}
