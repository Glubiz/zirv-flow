import { useEffect, useRef, useState } from "react";
import { Link, useNavigate, useParams } from "react-router";

import { client, describeError, unwrap, useResource, type components, type ErrorSummary } from "../../api";
import {
  Button,
  EmptyState,
  ErrorState,
  formatAgo,
  formatDateTime,
  InlineError,
  LoadingState,
  PageHeader,
  Panel,
  RelatedPanel,
  ResourceView,
  shortId,
  TabNav,
  useTabParam,
} from "../../ui";
import { AssistantToggle, useAssistantAccess, useAssistantDock } from "../assistant";
import { useSegmentAccess } from "./access";
import { fromDefinition } from "./ast";
import { useCatalog } from "./catalog";
import { namesOf } from "./describe";
import { cadenceLabel, causeLabel } from "./labels";
import { builtFrom, describeDefinition, usedBy } from "./overview";
import { DataAgeNote, personLabel } from "./LiveCount";
import { DefinitionView, SegmentForm } from "./SegmentForm";
import { usePaged } from "./usePaged";

type Segment = components["schemas"]["SegmentDetailBody"];
type Version = components["schemas"]["SegmentVersionBody"];

/** How long after a refresh is queued the page reads the segment again. */
export const REFRESH_RELOAD_MS = 2000;

const PAGE_SIZE = 50;

const TABS = [
  { id: "overview", label: "Overview" },
  { id: "rules", label: "Rules" },
  { id: "people", label: "People" },
  { id: "history", label: "History" },
];

export function SegmentPage() {
  const { ws = "", id = "" } = useParams();
  return <SegmentView key={`${ws}/${id}`} workspaceId={ws} segmentId={id} />;
}

function SegmentView({ workspaceId, segmentId }: { workspaceId: string; segmentId: string }) {
  const { canWrite, canUseAssistant } = useSegmentAccess(workspaceId);
  const { enabled: assistantOn, reload: reloadAssistant } = useAssistantAccess(workspaceId, canUseAssistant);
  const dock = useAssistantDock();
  const [dockSlot, setDockSlot] = useState<HTMLElement | null>(null);
  const segment = useResource(
    () =>
      unwrap(client.GET("/api/v1/workspaces/{ws}/segments/{id}", { params: { path: { ws: workspaceId, id: segmentId } } })),
    [workspaceId, segmentId],
  );
  const navigate = useNavigate();
  const { active: tab, hrefOf } = useTabParam(TABS);
  /** The dock lives beside the rules, so asking for it elsewhere goes to the rules with it open. */
  function toggleAssistant() {
    if (tab === "rules") {
      dock.toggle();
      return;
    }
    const next = new URLSearchParams(hrefOf("rules"));
    next.set("assistant", "1");
    void navigate(`?${next.toString()}`);
  }
  const crumb = { group: "Audience", section: "Segments", to: `/w/${workspaceId}/segments` };

  if (segment.status === "loading") {
    return (
      <div className="page">
        <PageHeader title="Segment" breadcrumb={crumb} />
        <LoadingState label="Loading the segment" />
      </div>
    );
  }
  if (segment.status === "error") {
    return (
      <div className="page">
        <PageHeader title="Segment" breadcrumb={crumb} />
        <Panel flush>
          <ErrorState {...describeError(segment.error)} onRetry={segment.reload} />
        </Panel>
      </div>
    );
  }
  const data = segment.data;

  return (
    <div className="dock-layout">
    <div className="page">
      <PageHeader
        title={data.name}
        breadcrumb={crumb}
        status={`${data.member_count} ${data.member_count === 1 ? "person" : "people"}`}
        lede={data.description || undefined}
        primary={
          canWrite ? (
            <Button variant="primary" onClick={() => void navigate(hrefOf("rules"))}>
              Edit rules
            </Button>
          ) : null
        }
        secondary={canWrite ? <RefreshButton workspaceId={workspaceId} segmentId={segmentId} onReload={segment.reload} /> : null}
        actions={assistantOn ? <AssistantToggle open={dock.open && tab === "rules"} onToggle={toggleAssistant} /> : null}
      />
      <dl className="facts">
        <div>
          <dt>People</dt>
          <dd>{data.member_count}</dd>
        </div>
        <div>
          <dt>Last computed</dt>
          <dd>{data.computed_at ? `computed ${formatAgo(data.computed_at)}` : "not computed yet"}</dd>
        </div>
        <div>
          <dt>Kept up to date</dt>
          <dd>re-evaluated {cadenceLabel(data.cadence_minutes)}</dd>
        </div>
        <div>
          <dt>Current version</dt>
          <dd>{data.current_version}</dd>
        </div>
      </dl>
      <TabNav label="Segment sections" tabs={TABS} />
      {tab === "overview" ? <Overview workspaceId={workspaceId} segment={data} canWrite={canWrite} editHref={hrefOf("rules")} /> : null}
      <div hidden={tab !== "rules"}>
        <SegmentForm
          workspaceId={workspaceId}
          segment={data}
          canWrite={canWrite}
          assistant={assistantOn}
          onAssistantGone={reloadAssistant}
          dockVisible={tab === "rules"}
          dockSlot={dockSlot}
          onSaved={() => segment.reload()}
        />
      </div>
      {tab === "people" ? <MembersPanel workspaceId={workspaceId} segmentId={segmentId} /> : null}
      {tab === "history" ? (
        <div className="stack">
          <HistoryPanel workspaceId={workspaceId} segmentId={segmentId} />
          <VersionsPanel workspaceId={workspaceId} segment={data} />
        </div>
      ) : null}
    </div>
    <div className="dock-slot" ref={setDockSlot} />
    </div>
  );
}

function Overview({ workspaceId, segment, canWrite, editHref }: { workspaceId: string; segment: Segment; canWrite: boolean; editHref: string }) {
  const catalog = useCatalog(workspaceId);
  const lists = catalog.status === "ready" ? catalog.catalog.lists : [];
  const segments = catalog.status === "ready" ? catalog.catalog.segments : [];
  const used = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/segments/{id}/where-used", {
          params: { path: { ws: workspaceId, id: segment.id } },
        }),
      ),
    [workspaceId, segment.id],
  );
  const sentence = describeDefinition(fromDefinition(segment.current.definition), namesOf(lists, segments));
  return (
    <div className="detail-grid">
      <Panel title="Who is in it">
        <div className="stack-tight">
          <p>{sentence}</p>
          {canWrite ? <Link to={editHref}>Edit rules</Link> : null}
        </div>
      </Panel>
      <RelatedPanel
        groups={[
          ...builtFrom(segment.current.deps, workspaceId, lists, segments, segment.id),
          ...usedBy(used.status === "ready" ? used.data : null, workspaceId),
        ]}
      />
    </div>
  );
}

function RefreshButton({ workspaceId, segmentId, onReload }: { workspaceId: string; segmentId: string; onReload: () => void }) {
  const [state, setState] = useState<"idle" | "busy" | "queued">("idle");
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const reload = useRef(onReload);

  useEffect(() => {
    reload.current = onReload;
  });
  useEffect(
    () => () => {
      if (timer.current !== null) clearTimeout(timer.current);
    },
    [],
  );

  async function refresh() {
    setState("busy");
    setFailure(null);
    try {
      await unwrap(
        client.POST("/api/v1/workspaces/{ws}/segments/{id}/refresh", { params: { path: { ws: workspaceId, id: segmentId } } }),
      );
      setState("queued");
      timer.current = setTimeout(() => {
        reload.current();
        setState("idle");
      }, REFRESH_RELOAD_MS);
    } catch (error) {
      setFailure(describeError(error));
      setState("idle");
    }
  }

  return (
    <div className="stack-tight">
      <Button onClick={() => void refresh()} disabled={state !== "idle"}>
        {state === "busy" ? "Refreshing" : "Refresh now"}
      </Button>
      {state === "queued" ? (
        <p className="faint" role="status">
          Refresh queued
        </p>
      ) : null}
      {failure ? <InlineError {...failure} /> : null}
    </div>
  );
}

function MoreButton({
  hasMore,
  loading,
  error,
  onMore,
}: {
  hasMore: boolean;
  loading: boolean;
  error: unknown;
  onMore: () => void;
}) {
  if (!hasMore && error === null) return null;
  return (
    <div className="panel-footer">
      {error !== null ? <InlineError {...describeError(error)} /> : null}
      {hasMore ? (
        <Button onClick={onMore} disabled={loading}>
          {loading ? "Loading" : "Load more"}
        </Button>
      ) : null}
    </div>
  );
}

function MembersPanel({ workspaceId, segmentId }: { workspaceId: string; segmentId: string }) {
  const members = usePaged(
    (cursor: string | undefined) =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/segments/{id}/members", {
          params: { path: { ws: workspaceId, id: segmentId }, query: { limit: PAGE_SIZE, ...(cursor ? { cursor } : {}) } },
        }),
      ),
    [workspaceId, segmentId],
  );
  return (
    <Panel flush>
      <ResourceView
        resource={members.resource}
        loadingLabel="Loading members"
        isEmpty={(data) => data.items.length === 0}
        empty={
          <EmptyState title="No one is in this segment">
            Either no one matches the definition, or it has not been computed yet.
          </EmptyState>
        }
      >
        {(data) => (
          <>
            <div className="panel-pad">
              <p className="muted">
                {data.first.computed_at ? `Computed ${formatDateTime(data.first.computed_at)}.` : "Not computed yet."}
              </p>
              <DataAgeNote age={data.first.data_age} />
            </div>
            <div className="table-scroll">
              <table className="table">
                <caption>People in this segment</caption>
                <thead>
                  <tr>
                    <th scope="col">Person</th>
                    <th scope="col">Joined</th>
                  </tr>
                </thead>
                <tbody>
                  {data.items.map((member) => (
                    <tr key={member.id}>
                      <td className="strong">
                        <Link to={`/w/${workspaceId}/people/${member.id}`}>{personLabel(member.id, member.display)}</Link>
                      </td>
                      <td>{formatDateTime(member.entered_at)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </>
        )}
      </ResourceView>
      {members.resource.status === "ready" ? (
        <MoreButton hasMore={members.hasMore} loading={members.loadingMore} error={members.moreError} onMore={members.loadMore} />
      ) : null}
    </Panel>
  );
}

function HistoryPanel({ workspaceId, segmentId }: { workspaceId: string; segmentId: string }) {
  const history = usePaged(
    (cursor: string | undefined) =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/segments/{id}/history", {
          params: { path: { ws: workspaceId, id: segmentId }, query: { limit: PAGE_SIZE, ...(cursor ? { cursor } : {}) } },
        }),
      ),
    [workspaceId, segmentId],
  );
  return (
    <Panel flush>
      <ResourceView
        resource={history.resource}
        loadingLabel="Loading history"
        isEmpty={(data) => data.items.length === 0}
        empty={<EmptyState title="No history yet">People are listed here as they join and leave the segment.</EmptyState>}
      >
        {(data) => (
          <div className="table-scroll">
            <table className="table">
              <caption>When people joined and left, newest first</caption>
              <thead>
                <tr>
                  <th scope="col">Person</th>
                  <th scope="col">What happened</th>
                  <th scope="col">Why</th>
                  <th scope="col" className="num">
                    Version
                  </th>
                  <th scope="col">When</th>
                </tr>
              </thead>
              <tbody>
                {data.items.map((item, index) => (
                  <tr key={`${item.person_id}:${item.at}:${item.kind}:${index}`}>
                    <td className="strong">
                      <Link to={`/w/${workspaceId}/people/${item.person_id}`}>{personLabel(item.person_id, item.display)}</Link>
                    </td>
                    <td>{item.kind === "entered" ? "Joined" : "Left"}</td>
                    <td>{causeLabel(item.cause)}</td>
                    <td className="num">{item.version}</td>
                    <td>{formatDateTime(item.at)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </ResourceView>
      {history.resource.status === "ready" ? (
        <MoreButton hasMore={history.hasMore} loading={history.loadingMore} error={history.moreError} onMore={history.loadMore} />
      ) : null}
    </Panel>
  );
}

function authorOf(version: Version): string {
  return version.created_by ? `user ${shortId(version.created_by)}` : "an API key or the system";
}

function VersionsPanel({ workspaceId, segment }: { workspaceId: string; segment: Segment }) {
  const versions = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/segments/{id}/versions", { params: { path: { ws: workspaceId, id: segment.id } } }),
      ),
    [workspaceId, segment.id],
  );
  const [chosen, setChosen] = useState<number | null>(null);

  return (
    <div className="stack">
      <Panel flush>
        <ResourceView
          resource={versions}
          loadingLabel="Loading versions"
          isEmpty={(data) => data.length === 0}
          empty={<EmptyState title="No versions">This segment has no saved version.</EmptyState>}
        >
          {(data) => (
            <div className="table-scroll">
              <table className="table">
                <caption>Every saved version, newest first</caption>
                <thead>
                  <tr>
                    <th scope="col" className="num">
                      Version
                    </th>
                    <th scope="col">Saved by</th>
                    <th scope="col">Saved</th>
                    <th scope="col">
                      <span className="visually-hidden">Actions</span>
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {data.map((version) => (
                    <tr key={version.version}>
                      <td className="num strong">{version.version}</td>
                      <td>{authorOf(version)}</td>
                      <td>{formatDateTime(version.created_at)}</td>
                      <td className="num">
                        <Button
                          onClick={() => setChosen(version.version)}
                          aria-pressed={chosen === version.version}
                          aria-label={`Show version ${version.version}`}
                        >
                          Show
                        </Button>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </ResourceView>
      </Panel>
      {versions.status === "ready" && chosen !== null ? (
        <ChosenVersion workspaceId={workspaceId} segment={segment} versions={versions.data} number={chosen} />
      ) : null}
    </div>
  );
}

function ChosenVersion({
  workspaceId,
  segment,
  versions,
  number,
}: {
  workspaceId: string;
  segment: Segment;
  versions: Version[];
  number: number;
}) {
  const version = versions.find((entry) => entry.version === number);
  if (!version) return null;
  return (
    <Panel title={`Version ${version.version}`}>
      <div className="stack">
        <p className="notice" role="status">
          This is version {version.version}; the current version is {segment.current_version}. It is shown here to read, not to edit.
        </p>
        <p className="faint">Saved {formatDateTime(version.created_at)} by {authorOf(version)}.</p>
        <DefinitionView workspaceId={workspaceId} definition={fromDefinition(version.definition)} selfId={segment.id} />
      </div>
    </Panel>
  );
}
