import { useId, useState, type KeyboardEvent } from "react";
import { useNavigate } from "react-router";

import { Button, Dialog, EmptyState, TextField } from "../ui";
import { destinationUrl, type Destination } from "./navigation";

export function PagePalette({ items, base, onClose }: { items: readonly Destination[]; base: string | null; onClose: () => void }) {
  const navigate = useNavigate();
  const [query, setQuery] = useState("");
  const [selected, setSelected] = useState(0);
  const listId = useId();
  const words = query.toLocaleLowerCase().trim().split(/\s+/);
  const results = items.filter((item) => destinationUrl(item, base) !== null && words.every((word) => `${item.label} ${item.group} ${item.route}`.toLocaleLowerCase().includes(word)));
  const active = Math.min(selected, Math.max(0, results.length - 1));

  function open(item: Destination) {
    const to = destinationUrl(item, base);
    if (to === null) return;
    onClose();
    void navigate(to);
  }

  function onKeyDown(event: KeyboardEvent<HTMLInputElement>) {
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      if (results.length > 0) {
        const next = (active + (event.key === "ArrowDown" ? 1 : -1) + results.length) % results.length;
        setSelected(next);
        document.getElementById(`${listId}-${next}`)?.scrollIntoView({ block: "nearest" });
      }
    } else if (event.key === "Enter") {
      event.preventDefault();
      const item = results[active];
      if (item) open(item);
    }
  }

  return (
    <Dialog title="Go to a page" onClose={onClose}>
      <TextField
        label="Search pages"
        value={query}
        onChange={(event) => { setQuery(event.target.value); setSelected(0); }}
        onKeyDown={onKeyDown}
        role="combobox"
        aria-expanded="true"
        aria-autocomplete="list"
        aria-controls={listId}
        aria-activedescendant={results.length > 0 ? `${listId}-${active}` : undefined}
        autoComplete="off"
      />
      <ul className="palette-results" id={listId} role="listbox" aria-label="Pages">
        {results.map((item, index) => (
          <li id={`${listId}-${index}`} key={item.route} role="option" aria-label={`${item.label}${item.group ? ` ${item.group}` : ""}`} aria-selected={index === active} onClick={() => open(item)}>
            <span>{item.label}</span><span className="muted">{item.group}</span>
          </li>
        ))}
      </ul>
      {results.length === 0 ? <EmptyState title="No pages match">Try a page or group name.</EmptyState> : null}
      <div className="dialog-actions"><Button onClick={onClose}>Close</Button></div>
    </Dialog>
  );
}
