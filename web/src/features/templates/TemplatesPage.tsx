import { useCallback, useEffect, useMemo, useState } from "react";
import { Link, useNavigate, useParams, useSearchParams } from "react-router";

import { ApiError, client, describeError, unwrap, useResource, type ErrorSummary } from "../../api";
import { Button, EmptyState, InlineError, LibraryNav, PageHeader, Pill, ResourceView } from "../../ui";
import { useThemes } from "../themes";
import { useTemplateAccess } from "./access";
import { duplicateTemplate } from "./duplicate";
import { ImportDialog } from "./ImportDialog";
import {
  checksLabel,
  editedAgo,
  editedLabel,
  flowCount,
  isFiltered,
  kindLabel,
  listParams,
  readFilters,
  SORTS,
  statusCounts,
  statusKey,
  statusLabel,
  STATUS_CHIPS,
  themeLabel,
  usedLabel,
  writeFilters,
  type ChannelFilter,
  type Filters,
  type SortKey,
  type TemplateListItem,
} from "./library";
import { NewTemplateDialog } from "./NewTemplateDialog";
import { ArchiveTemplateDialog, TemplateMenu } from "./TemplateActions";
import { TemplateThumb, thumbTokens, type ThumbTokens } from "./TemplateThumb";

const SEARCH_DELAY_MS = 300;
const NONE: never[] = [];

export function TemplatesPage() {
  const { ws = "" } = useParams();
  return <TemplateLibrary key={ws} workspaceId={ws} />;
}

function TemplateLibrary({ workspaceId }: { workspaceId: string }) {
  const access = useTemplateAccess(workspaceId);
  const navigate = useNavigate();
  const [params, setParams] = useSearchParams();
  const filters = readFilters(params);
  const [text, setText] = useState(filters.q);
  const [adding, setAdding] = useState(false);
  const [importing, setImporting] = useState(false);
  const [archiving, setArchiving] = useState<TemplateListItem | null>(null);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [copying, setCopying] = useState<string | null>(null);

  const query = listParams(filters);
  const list = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/templates", { params: { path: { ws: workspaceId }, query } })),
    [workspaceId, JSON.stringify(query)],
  );
  // One call for everything: the chip counts, the first-run check and the templates a new one can start from.
  const everything = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/templates", { params: { path: { ws: workspaceId }, query: { archived: true } } })),
    [workspaceId],
  );
  const themes = useThemes(workspaceId);

  const update = useCallback(
    (patch: Partial<Filters>) => {
      setParams((current) => writeFilters({ ...readFilters(current), ...patch }), { replace: true });
    },
    [setParams],
  );

  // The search waits for a pause in typing before it reaches the URL and the server.
  useEffect(() => {
    const term = text.trim();
    if (term === filters.q) return;
    const timer = setTimeout(() => update({ q: term }), SEARCH_DELAY_MS);
    return () => clearTimeout(timer);
  }, [text, filters.q, update]);

  function clearFilters() {
    setText("");
    update({ q: "", status: "", channel: "", theme: "" });
  }

  function reload() {
    list.reload();
    everything.reload();
  }

  const allItems = everything.status === "ready" ? everything.data : NONE;
  const counts = statusCounts(allItems);
  const known = useMemo(() => new Set(allItems.map((item) => item.name.toLowerCase())), [allItems]);
  const themeOptions = themes.status === "ready" ? themes.data : NONE;

  const tokensOf = useMemo(() => {
    const byId = new Map<string, ThumbTokens>();
    let fallback: ThumbTokens | null = null;
    for (const theme of themeOptions) {
      const tokens = thumbTokens(theme.tokens);
      byId.set(theme.id, tokens);
      if (theme.is_default) fallback = tokens;
    }
    return (item: TemplateListItem): ThumbTokens => {
      if (item.theme === "default") return fallback ?? thumbTokens(null);
      if (item.theme === "none") return thumbTokens(null);
      return byId.get(item.theme.id) ?? thumbTokens(null);
    };
  }, [themeOptions]);

  async function copy(item: TemplateListItem) {
    setCopying(item.id);
    setFailure(null);
    try {
      const made = await duplicateTemplate(workspaceId, item.id, known);
      void navigate(`/w/${workspaceId}/templates/${made.id}`);
    } catch (error) {
      setFailure(error instanceof ApiError ? describeError(error) : { title: "The copy could not be made", detail: error instanceof Error ? error.message : "", traceId: null });
      setCopying(null);
    }
  }

  const empty = everything.status === "ready" && everything.data.length === 0;

  return (
    <div className="page">
      <PageHeader
        title="Template library"
        breadcrumb={{ group: "Messaging", section: "Templates" }}
        lede="The emails and webhook payloads your flows send. Themes, snippets and images are shared, so a change reaches every template that uses them."
        actions={
          access.canWrite ? (
            <>
              <Button onClick={() => setImporting(true)}>Import HTML or MJML</Button>
              <Button variant="primary" onClick={() => setAdding(true)}>
                New template
              </Button>
            </>
          ) : undefined
        }
      />
      <LibraryNav workspaceId={workspaceId} />
      <div className="tl-toolbar">
        <div className="field tl-search">
          <input
            className="input"
            type="search"
            aria-label="Search templates"
            placeholder="Search by name, subject or field"
            value={text}
            onChange={(event) => setText(event.target.value)}
          />
        </div>
        <div className="seg-tabs" role="group" aria-label="Status">
          {STATUS_CHIPS.map((chip) => (
            <button
              key={chip.id}
              type="button"
              className={filters.status === chip.id ? "seg seg-on" : "seg"}
              aria-pressed={filters.status === chip.id}
              aria-label={everything.status === "ready" ? `${chip.label} ${counts[chip.id]}` : chip.label}
              onClick={() => update({ status: chip.id })}
            >
              {chip.label}
              {everything.status === "ready" ? <span className="tl-count">{counts[chip.id]}</span> : null}
            </button>
          ))}
        </div>
        <select className="select tl-select" aria-label="Channel" value={filters.channel} onChange={(event) => update({ channel: event.target.value as ChannelFilter })}>
          <option value="">All channels</option>
          <option value="email">Email</option>
          <option value="webhook">Webhook</option>
        </select>
        <select className="select tl-select" aria-label="Theme" value={filters.theme} onChange={(event) => update({ theme: event.target.value })}>
          <option value="">All themes</option>
          <option value="default">Workspace default</option>
          <option value="none">No theme</option>
          {themeOptions.map((theme) => (
            <option key={theme.id} value={theme.id}>
              {theme.name}
            </option>
          ))}
        </select>
        <div className="tl-end">
          <select className="select tl-select" aria-label="Sort" value={filters.sort} onChange={(event) => update({ sort: event.target.value as SortKey })}>
            {SORTS.map((sort) => (
              <option key={sort.id} value={sort.id}>
                {sort.label}
              </option>
            ))}
          </select>
          <div className="seg-tabs" role="group" aria-label="View">
            <button type="button" className={filters.view === "cards" ? "seg seg-on" : "seg"} aria-pressed={filters.view === "cards"} onClick={() => update({ view: "cards" })}>
              Cards
            </button>
            <button type="button" className={filters.view === "list" ? "seg seg-on" : "seg"} aria-pressed={filters.view === "list"} onClick={() => update({ view: "list" })}>
              List
            </button>
          </div>
        </div>
      </div>
      {failure ? <InlineError {...failure} /> : null}
      <ResourceView
        resource={list}
        loadingLabel="Loading templates"
        isEmpty={(data) => data.length === 0}
        empty={
          empty ? (
            <EmptyState
              title="No templates yet"
              action={
                access.canWrite ? (
                  <Button variant="primary" onClick={() => setAdding(true)}>
                    New template
                  </Button>
                ) : undefined
              }
            >
              {access.canWrite ? "Make a template to use it in a send step." : "Members of the workspace make the templates."}
            </EmptyState>
          ) : (
            <EmptyState
              title="No templates match"
              action={
                isFiltered(filters) || text !== "" ? (
                  <Button onClick={clearFilters}>Clear search and filters</Button>
                ) : undefined
              }
            >
              Search looks at names, subjects and the fields a template uses.
            </EmptyState>
          )
        }
      >
        {(data) =>
          filters.view === "list" ? (
            <ListView
              workspaceId={workspaceId}
              items={data}
              access={access}
              copying={copying}
              onDuplicate={(item) => void copy(item)}
              onArchive={setArchiving}
            />
          ) : (
            <CardsView
              workspaceId={workspaceId}
              items={data}
              access={access}
              copying={copying}
              tokensOf={tokensOf}
              onDuplicate={(item) => void copy(item)}
              onArchive={setArchiving}
            />
          )
        }
      </ResourceView>
      {adding ? <NewTemplateDialog workspaceId={workspaceId} templates={allItems} onClose={() => setAdding(false)} /> : null}
      {importing ? <ImportDialog workspaceId={workspaceId} onClose={() => setImporting(false)} /> : null}
      {archiving ? (
        <ArchiveTemplateDialog
          workspaceId={workspaceId}
          item={archiving}
          onClose={() => setArchiving(null)}
          onDone={() => {
            setArchiving(null);
            reload();
          }}
        />
      ) : null}
    </div>
  );
}

interface ViewProps {
  workspaceId: string;
  items: TemplateListItem[];
  access: { canWrite: boolean; canAdmin: boolean };
  copying: string | null;
  onDuplicate: (item: TemplateListItem) => void;
  onArchive: (item: TemplateListItem) => void;
}

function menuOf(props: ViewProps, item: TemplateListItem) {
  return (
    <TemplateMenu
      workspaceId={props.workspaceId}
      item={item}
      canWrite={props.access.canWrite && props.copying === null}
      canAdmin={props.access.canAdmin}
      onDuplicate={() => props.onDuplicate(item)}
      onArchive={() => props.onArchive(item)}
    />
  );
}

function StatusPills({ item }: { item: TemplateListItem }) {
  const checks = checksLabel(item);
  return (
    <div className="tl-pills">
      <Pill>{statusLabel(item)}</Pill>
      {statusKey(item) === "changes" ? <Pill tone="attention">Draft changes</Pill> : null}
      <Pill tone={checks.tone}>{checks.text}</Pill>
    </div>
  );
}

function CardsView(props: ViewProps & { tokensOf: (item: TemplateListItem) => ThumbTokens }) {
  return (
    <ul className="tl-grid" aria-label="Templates of this workspace">
      {props.items.map((item) => (
        <li key={item.id} className="tl-card">
          <TemplateThumb item={item} tokens={props.tokensOf(item)} />
          <div className="tl-body">
            <div className="tl-head">
              <div className="tl-title">
                <Link className="tl-name" to={`/w/${props.workspaceId}/templates/${item.id}`}>
                  {item.name}
                </Link>
                <div className="hint">{kindLabel(item)}</div>
              </div>
              {menuOf(props, item)}
            </div>
            <StatusPills item={item} />
            <div className="tl-foot">
              <span>{usedLabel(item)}</span>
              <span>{editedLabel(item)}</span>
            </div>
          </div>
        </li>
      ))}
    </ul>
  );
}

function ListView(props: ViewProps) {
  return (
    <div className="panel panel-flush">
      <div className="table-scroll">
        <table className="table">
          <caption>Templates of this workspace</caption>
          <thead>
            <tr>
              <th scope="col">Name</th>
              <th scope="col">Status</th>
              <th scope="col">Checks</th>
              <th scope="col">Used in</th>
              <th scope="col">Theme</th>
              <th scope="col">Edited</th>
              <th scope="col">
                <span className="visually-hidden">Actions</span>
              </th>
            </tr>
          </thead>
          <tbody>
            {props.items.map((item) => {
              const checks = checksLabel(item);
              return (
                <tr key={item.id}>
                  <td className="strong">
                    <Link to={`/w/${props.workspaceId}/templates/${item.id}`}>{item.name}</Link>
                    <div className="hint">{kindLabel(item)}</div>
                  </td>
                  <td>
                    {statusLabel(item)}
                    {statusKey(item) === "changes" ? <div className="hint">Draft changes</div> : null}
                  </td>
                  <td>{checks.text}</td>
                  <td>{flowCount(item)}</td>
                  <td>{themeLabel(item)}</td>
                  <td>
                    {editedAgo(item)}
                    {item.edited_by_email ? <div className="hint">{item.edited_by_email}</div> : null}
                  </td>
                  <td>{menuOf(props, item)}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </div>
  );
}
