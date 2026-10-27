import type { ReactNode } from "react";
import { Link } from "react-router";

import { describeError, type components, type Resource } from "../../api";
import { ErrorState, LoadingState } from "../../ui";
import { formatDateTime } from "../../ui/format";
import type { TemplateUse, TemplateVersionSummary } from "./model";
import { themeChoice, type ThemeChoice } from "./versionUse";

type Stats = components["schemas"]["TemplateVersionStatsBody"];

/** What the history list shows for one row. */
export interface Row {
  key: "draft" | number;
  name: string;
  badge: string;
  note: string;
  when: string;
  live: boolean;
}

export function versionRow(version: TemplateVersionSummary, live: number | null, author: string): Row {
  return {
    key: version.version,
    name: `Version ${version.version}`,
    badge: version.version === live ? "Live" : `v${version.version}`,
    note: version.note ?? "No note",
    when: `${formatDateTime(version.published_at)} · ${author}`,
    live: version.version === live,
  };
}

/** The draft row: what the editor holds, and how far it has moved from the live version. */
export function draftRow(live: number | null, changes: number, savedAt: string): Row {
  const since = live === null ? "Not published yet" : changes === 0 ? `Same as v${live}` : `${changes} ${changes === 1 ? "change" : "changes"} since v${live}`;
  return { key: "draft", name: "Draft", badge: "Editing", note: since, when: `Saved ${formatDateTime(savedAt)}`, live: false };
}

export function HistoryList({ rows, selected, onSelect, empty }: { rows: readonly Row[]; selected: "draft" | number; onSelect: (key: "draft" | number) => void; empty: boolean }) {
  return (
    <section className="panel pb-panel hv-list" aria-labelledby="hv-history-title">
      <div>
        <h2 className="panel-title" id="hv-history-title">
          History
        </h2>
        <p className="faint">Published versions never change. Flows always send the newest one.</p>
      </div>
      <ul className="list-plain">
        {rows.map((row) => (
          <li key={row.key}>
            <button type="button" className={selected === row.key ? "hv-ver hv-ver-on" : "hv-ver"} aria-current={selected === row.key ? "true" : undefined} onClick={() => onSelect(row.key)}>
              <span className="row">
                <b>{row.name}</b>
                <span className={row.live ? "hv-chip hv-chip-live" : "hv-chip"}>{row.badge}</span>
              </span>
              <span>{row.note}</span>
              <span className="faint">{row.when}</span>
            </button>
          </li>
        ))}
      </ul>
      {empty ? <p className="faint">No published versions yet. Publish the draft to make version 1.</p> : null}
    </section>
  );
}

function Fact({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="hv-fact">
      <span className="faint">{label}</span>
      <span className="hv-fact-value">{children}</span>
    </div>
  );
}

function themeText(choice: ThemeChoice, name: string | null): string {
  if (choice.kind === "none") return "No theme";
  if (choice.kind === "default") return "Follows the default theme";
  return name ?? "A named theme";
}

interface DetailsProps {
  workspaceId: string;
  title: string;
  /** Null for the draft, which nobody published. */
  author: string | null;
  document: unknown;
  /** The version's stats; null for the draft, which has none. */
  stats: Resource<Stats | null> | null;
  themeName: string | null;
  snippets: readonly { id: string; name: string }[];
  snippetNames: ReadonlyMap<string, string>;
}

/** The facts of the selected version: who published it, what it sent, and the theme and snippets it names. */
export function DetailsPanel({ workspaceId, title, author, document, stats, themeName, snippets, snippetNames }: DetailsProps) {
  return (
    <section className="panel pb-panel" aria-labelledby="hv-details-title">
      <h2 className="panel-title" id="hv-details-title">
        {title}
      </h2>
      <Fact label="Published by">{author ?? "Not published"}</Fact>
      {stats === null ? null : <Fact label="Sent">{stats.status === "loading" ? "Counting" : stats.status === "error" ? "Could not count" : stats.data === null ? "—" : `${stats.data.messages.toLocaleString("en")} ${stats.data.messages === 1 ? "message" : "messages"}`}</Fact>}
      <Fact label="Theme">{themeText(themeChoice(document), themeName)}</Fact>
      <Fact label="Snippets">
        {snippets.length === 0 ? (
          "None"
        ) : (
          <span className="hv-links">
            {snippets.map((snippet) => (
              <Link key={snippet.id} to={`/w/${workspaceId}/snippets/${snippet.id}`}>
                {snippet.name}
              </Link>
            ))}
          </span>
        )}
      </Fact>
      {stats?.status === "error" ? <ErrorState {...describeError(stats.error)} onRetry={stats.reload} /> : null}
      {stats?.status === "ready" && stats.data !== null && stats.data.renders.length > 0 ? (
        <>
          <p className="faint">Each message records the theme and snippet versions it was sent with.</p>
          <ul className="list-plain hv-renders" aria-label="Versions it was sent with">
            {stats.data.renders.map((render, index) => {
              const used = Object.entries(render.snippet_versions as Record<string, number>).map(([id, version]) => `${snippetNames.get(id) ?? "A snippet"} v${version}`);
              const parts = [render.theme_version == null ? null : `Theme v${render.theme_version}`, ...used].filter((part) => part !== null);
              return (
                <li key={index} className="row">
                  <span className="row-grow">{parts.length === 0 ? "No theme or snippet" : parts.join(", ")}</span>
                  <span className="faint">{render.count.toLocaleString("en")}</span>
                </li>
              );
            })}
          </ul>
          {stats.data.more ? <p className="faint">More combinations than shown.</p> : null}
        </>
      ) : null}
          </section>
  );
}

/** The flows that name the template, and why archiving may be blocked. */
export function UsedByPanel({ workspaceId, uses }: { workspaceId: string; uses: Resource<TemplateUse[]> }) {
  return (
    <section className="panel pb-panel" aria-labelledby="hv-used-title">
      <h2 className="panel-title" id="hv-used-title">
        Used by
      </h2>
      {uses.status === "loading" ? <LoadingState label="Checking which flows use this template" /> : null}
      {uses.status === "error" ? <ErrorState {...describeError(uses.error)} onRetry={uses.reload} /> : null}
      {uses.status === "ready" && uses.data.length === 0 ? <p className="faint">No flow uses this template yet.</p> : null}
      {uses.status === "ready" && uses.data.length > 0 ? (
        <ul className="list-plain" aria-label="Flows that use this template">
          {uses.data.map((use) => (
            <li key={use.flow_id}>
              <Link className="pb-flow" to={`/w/${workspaceId}/flows/${use.flow_id}`}>
                <span className="strong">{use.flow_name}</span>
                <span className="faint">{use.active ? "Active" : use.flow_state}</span>
              </Link>
            </li>
          ))}
        </ul>
      ) : null}
      <p className="faint">Archiving is blocked while an active flow sends this template.</p>
    </section>
  );
}
