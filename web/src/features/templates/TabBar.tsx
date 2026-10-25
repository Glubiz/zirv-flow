import type { KeyboardEvent } from "react";

interface Props<T extends string> {
  label: string;
  tabs: readonly { id: T; label: string }[];
  tab: T;
  onTab: (next: T) => void;
  /** `page` is the pill row of a page header; `field` the segmented row above a pane. */
  variant?: "field" | "page";
}

/** A tab row with roving focus: the arrow keys move to the next or previous tab and select it. */
export function TabBar<T extends string>({ label, tabs, tab, onTab, variant = "field" }: Props<T>) {
  function onKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    if (event.key !== "ArrowRight" && event.key !== "ArrowLeft") return;
    const at = tabs.findIndex((entry) => entry.id === tab);
    const next = tabs[(at + (event.key === "ArrowRight" ? 1 : tabs.length - 1)) % tabs.length];
    if (next) {
      onTab(next.id);
      event.currentTarget.querySelector<HTMLElement>(`[data-tab="${next.id}"]`)?.focus();
    }
  }
  const page = variant === "page";
  return (
    <div className={page ? "fl-tabs" : "seg-tabs"} role="tablist" aria-label={label} onKeyDown={onKeyDown}>
      {tabs.map((entry) => (
        <button
          key={entry.id}
          type="button"
          role="tab"
          data-tab={entry.id}
          className={page ? "fl-tab" : entry.id === tab ? "seg seg-on" : "seg"}
          aria-selected={entry.id === tab}
          tabIndex={entry.id === tab ? 0 : -1}
          onClick={() => onTab(entry.id)}
        >
          {entry.label}
        </button>
      ))}
    </div>
  );
}
