import { useEffect, useId, useRef, useState, type KeyboardEvent } from "react";
import { useNavigate } from "react-router";

import { client, unwrap, useResource, type components, type Resource } from "../api";
import { Button, Dialog, EmptyState, TextField } from "../ui";
import { destinationUrl, type Destination } from "./navigation";
import { GROUP_LIMIT, identifierKind, matchNames } from "./paletteSearch";

type Flow = components["schemas"]["FlowBody"];
type Segment = components["schemas"]["SegmentBody"];
type Template = components["schemas"]["TemplateListItemBody"];
type Person = components["schemas"]["PersonHitBody"];

const DEBOUNCE_MS = 200;
const FLOW_STATE: Record<Flow["state"], string> = { draft: "Draft", published: "Live", paused: "Paused", archived: "Archived" };

interface Entry {
  key: string;
  label: string;
  state?: string;
  aria: string;
  to?: string;
  find?: boolean;
}

interface Group {
  name: string;
  entries: Entry[];
  note?: string;
}

type PeopleState =
  | { query: string; status: "loading" }
  | { query: string; status: "error" }
  | { query: string; status: "ready"; people: Person[] };

function objectGroup<T extends { id: string; name: string }>(
  name: string,
  noun: string,
  path: string,
  base: string | null,
  resource: Resource<T[]>,
  query: string,
  stateOf: (item: T) => string,
): Group {
  if (resource.status === "error") return { name, entries: [], note: `Could not search ${name.toLocaleLowerCase()}` };
  const items = resource.status === "ready" && base !== null ? matchNames(resource.data, query) : [];
  return {
    name,
    entries: items.map((item) => {
      const state = stateOf(item);
      return { key: `${path}-${item.id}`, label: item.name, state, aria: `${item.name}, ${noun}, ${state.toLocaleLowerCase()}`, to: `${base}/${path}/${item.id}` };
    }),
  };
}

export function PagePalette({ items, base, workspaceId, canSearchPeople, onClose }: {
  items: readonly Destination[];
  base: string | null;
  workspaceId: string | null;
  canSearchPeople: boolean;
  onClose: () => void;
}) {
  const navigate = useNavigate();
  const [query, setQuery] = useState("");
  const [settled, setSettled] = useState("");
  const [selected, setSelected] = useState(0);
  const [people, setPeople] = useState<PeopleState | null>(null);
  const search = useRef(0);
  const listId = useId();

  const flows = useResource<Flow[]>(() => workspaceId === null ? Promise.resolve([]) : unwrap(client.GET("/api/v1/workspaces/{ws}/flows", { params: { path: { ws: workspaceId } } })), [workspaceId]);
  const segments = useResource<Segment[]>(() => workspaceId === null ? Promise.resolve([]) : unwrap(client.GET("/api/v1/workspaces/{ws}/segments", { params: { path: { ws: workspaceId } } })), [workspaceId]);
  const templates = useResource<Template[]>(() => workspaceId === null ? Promise.resolve([]) : unwrap(client.GET("/api/v1/workspaces/{ws}/templates", { params: { path: { ws: workspaceId } } })), [workspaceId]);

  useEffect(() => {
    const timer = setTimeout(() => setSettled(query), DEBOUNCE_MS);
    return () => clearTimeout(timer);
  }, [query]);

  useEffect(() => () => { search.current += 1; }, []);

  const trimmed = query.trim();
  const words = query.toLocaleLowerCase().trim().split(/\s+/);
  const pages: Entry[] = items
    .filter((item) => destinationUrl(item, base) !== null && words.every((word) => `${item.label} ${item.group} ${item.route}`.toLocaleLowerCase().includes(word)))
    .map((item) => ({ key: `page-${item.route}`, label: item.label, state: item.group, aria: `${item.label}${item.group ? ` ${item.group}` : ""}`, to: destinationUrl(item, base) ?? undefined }));

  const groups: Group[] = [{ name: "Pages", entries: pages }];
  const waiting = trimmed !== "" && (settled !== query || flows.status === "loading" || segments.status === "loading" || templates.status === "loading");
  if (trimmed !== "") {
    groups.push(
      objectGroup("Flows", "flow", "flows", base, flows, settled, (flow) => FLOW_STATE[flow.state]),
      objectGroup("Segments", "segment", "segments", base, segments, settled, (segment) => String(segment.member_count)),
      objectGroup("Templates", "template", "templates", base, templates, settled, (template) => template.current_version == null ? "Draft" : "Published"),
    );
    const kind = identifierKind(trimmed);
    if (canSearchPeople && workspaceId !== null && base !== null && kind !== null) {
      const state = people?.query === trimmed ? people : null;
      if (state?.status === "ready") {
        groups.push({
          name: "People",
          entries: state.people.slice(0, GROUP_LIMIT).map((person) => {
            const label = person.display?.value ?? "Person";
            return { key: `person-${person.id}`, label, aria: `${label}, person`, to: `${base}/people/${person.id}` };
          }),
          note: state.people.length === 0 ? "No people found" : undefined,
        });
      } else if (state?.status === "error") {
        groups.push({ name: "People", entries: [], note: "Could not search people" });
      } else if (state?.status === "loading") {
        groups.push({ name: "People", entries: [], note: "Searching" });
      } else {
        groups.push({ name: "People", entries: [{ key: "find-person", label: `Find person ${trimmed}`, aria: `Find person ${trimmed}`, find: true }] });
      }
    }
  }

  const shown = groups.filter((group) => group.entries.length > 0 || group.note !== undefined);
  const options = shown.flatMap((group) => group.entries);
  const active = Math.min(selected, Math.max(0, options.length - 1));

  function open(entry: Entry, newTab = false) {
    if (entry.find) {
      findPeople();
      return;
    }
    if (entry.to === undefined) return;
    onClose();
    if (newTab) window.open(entry.to, "_blank", "noopener");
    else void navigate(entry.to);
  }

  function findPeople() {
    const kind = identifierKind(trimmed);
    if (kind === null || workspaceId === null) return;
    const token = ++search.current;
    setPeople({ query: trimmed, status: "loading" });
    unwrap(client.POST("/api/v1/workspaces/{ws}/people/search", { params: { path: { ws: workspaceId } }, body: { identifier: { kind, value: trimmed }, limit: GROUP_LIMIT } })).then(
      (page) => { if (search.current === token) setPeople({ query: trimmed, status: "ready", people: page.people }); },
      () => { if (search.current === token) setPeople({ query: trimmed, status: "error" }); },
    );
  }

  function move(next: number) {
    setSelected(next);
    document.getElementById(`${listId}-${next}`)?.scrollIntoView({ block: "nearest" });
  }

  function onKeyDown(event: KeyboardEvent<HTMLInputElement>) {
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      if (options.length > 0) move((active + (event.key === "ArrowDown" ? 1 : -1) + options.length) % options.length);
    } else if (event.key === "Home" || event.key === "End") {
      event.preventDefault();
      if (options.length > 0) move(event.key === "Home" ? 0 : options.length - 1);
    } else if (event.key === "Enter") {
      event.preventDefault();
      const entry = options[active];
      if (entry) open(entry, event.metaKey || event.ctrlKey);
    }
  }

  let index = 0;
  const count = trimmed === "" ? "" : `${options.length} ${options.length === 1 ? "result" : "results"}`;
  const empty = options.length === 0 && !waiting && shown.every((group) => group.note === undefined);

  return (
    <Dialog title="Go to a page" onClose={onClose}>
      <TextField
        label="Search pages"
        value={query}
        onChange={(event) => { setQuery(event.target.value); setSelected(0); }}
        onKeyDown={onKeyDown}
        placeholder="Search pages, flows, segments, templates, people"
        role="combobox"
        aria-expanded="true"
        aria-autocomplete="list"
        aria-controls={listId}
        aria-activedescendant={options.length > 0 ? `${listId}-${active}` : undefined}
        autoComplete="off"
      />
      <ul className="palette-results" id={listId} role="listbox" aria-label="Results">
        {shown.map((group) => (
          <li key={group.name} role="presentation">
            <ul role="group" aria-label={group.name} className="palette-group">
              <li role="presentation" className="palette-heading muted" aria-hidden="true">{group.name}</li>
              {group.entries.map((entry) => {
                const position = index++;
                return (
                  <li id={`${listId}-${position}`} key={entry.key} role="option" aria-label={entry.aria} aria-selected={position === active} onClick={() => open(entry)}>
                    <span>{entry.label}</span><span className="muted">{entry.state}</span>
                  </li>
                );
              })}
              {group.note ? <li role="presentation" className="muted palette-note">{group.note}</li> : null}
            </ul>
          </li>
        ))}
      </ul>
      {waiting ? <p className="muted palette-note" aria-live="polite">Searching</p> : null}
      <div className="visually-hidden" role="status" aria-live="polite">{count}</div>
      {empty ? (
        <EmptyState title="No pages match">{identifierKind(trimmed) === null ? `No results for “${trimmed}”` : "Try a page or group name."}</EmptyState>
      ) : null}
      <div className="dialog-actions"><Button onClick={onClose}>Close</Button></div>
    </Dialog>
  );
}
