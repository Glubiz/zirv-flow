import { useId, type KeyboardEvent } from "react";
import { Link, useParams, useSearchParams } from "react-router";

import { client, describeError, unwrap, useResource, type components } from "../../api";
import { EmptyState, ErrorState, formatDateTime, HiddenValue, kindLabel, LoadingState, PageHeader, Panel, shortId } from "../../ui";
import { ConsentPanel } from "../consent";
import { PersonListsPanel } from "../lists";
import { useWorkspaceAccess } from "../members";
import { SegmentExplainPanel } from "../segments";
import { MessageLog } from "./MessageLog";
import { Timeline } from "./Timeline";
import { typeLabel } from "./TraitDialog";

type Profile = components["schemas"]["ProfileBody"];
type Identity = components["schemas"]["ProfileIdentityBody"];
type ProfileTrait = components["schemas"]["ProfileTraitBody"];

export function ProfilePage() {
  const { ws = "", id = "" } = useParams();
  return <ProfileView key={`${ws}/${id}`} workspaceId={ws} requestedId={id} />;
}

function ProfileView({ workspaceId, requestedId }: { workspaceId: string; requestedId: string }) {
  const profile = useResource(
    () =>
      unwrap(client.GET("/api/v1/workspaces/{ws}/people/{id}", { params: { path: { ws: workspaceId, id: requestedId } } })),
    [workspaceId, requestedId],
  );
  const [params, setParams] = useSearchParams();
  const tab: PersonTab = params.get("tab") === "messages" ? "messages" : "profile";
  const tabsId = useId();
  const access = useWorkspaceAccess(workspaceId);
  const canWrite = access.status === "ready" && (access.data.canManage || access.data.role !== null);

  const back = (
    <p>
      <Link to={`/w/${workspaceId}/people`}>All people</Link>
    </p>
  );

  if (profile.status === "loading") {
    return (
      <div className="page">
        {back}
        <LoadingState label="Loading the profile" />
      </div>
    );
  }
  if (profile.status === "error") {
    return (
      <div className="page">
        {back}
        <Panel flush>
          <ErrorState {...describeError(profile.error)} onRetry={profile.reload} />
        </Panel>
      </div>
    );
  }
  const data = profile.data;

  return (
    <div className="page">
      {back}
      <PageHeader title={titleOf(data)} lede={ledeOf(data)} />
      {data.merged_from ? (
        <p className="notice" role="status">
          The person you opened, {shortId(data.merged_from)}, was merged into this one. This is the combined profile.
        </p>
      ) : null}
      <PersonTabs
        id={tabsId}
        tab={tab}
        onSelect={(next) => {
          const updated = new URLSearchParams(params);
          if (next === "messages") updated.set("tab", "messages");
          else updated.delete("tab");
          setParams(updated, { replace: true });
        }}
      />
      {tab === "messages" ? (
        <div role="tabpanel" id={`${tabsId}-messages-panel`} aria-labelledby={`${tabsId}-messages`}>
          <MessageLog workspaceId={workspaceId} personId={data.id} />
        </div>
      ) : (
        <div role="tabpanel" id={`${tabsId}-profile-panel`} aria-labelledby={`${tabsId}-profile`} className="stack">
          <div className="profile-layout">
            <div className="stack">
              <IdentitiesPanel profile={data} />
              <TraitsPanel traits={data.traits} />
              {data.merges.length > 0 ? <MergesPanel workspaceId={workspaceId} profile={data} /> : null}
            </div>
            <div className="stack">
              <ConsentPanel workspaceId={workspaceId} personId={data.id} canWrite={canWrite} />
              <PersonListsPanel workspaceId={workspaceId} personId={data.id} />
              <SegmentExplainPanel workspaceId={workspaceId} personId={data.id} />
            </div>
          </div>
          <Timeline workspaceId={workspaceId} personId={data.id} />
        </div>
      )}
    </div>
  );
}

type PersonTab = "profile" | "messages";

const TABS: readonly { id: PersonTab; label: string }[] = [
  { id: "profile", label: "Profile" },
  { id: "messages", label: "Messages" },
];

/** The views of a person: what is known about them, and the messages sent to them. */
function PersonTabs({ id, tab, onSelect }: { id: string; tab: PersonTab; onSelect: (tab: PersonTab) => void }) {
  function onKeyDown(event: KeyboardEvent<HTMLButtonElement>) {
    const step = event.key === "ArrowRight" ? 1 : event.key === "ArrowLeft" ? -1 : 0;
    if (step === 0) return;
    const next = TABS[(TABS.findIndex((item) => item.id === tab) + step + TABS.length) % TABS.length];
    if (!next) return;
    event.preventDefault();
    onSelect(next.id);
    document.getElementById(`${id}-${next.id}`)?.focus();
  }
  return (
    <div role="tablist" aria-label="Person views" className="seg-tabs">
      {TABS.map((item) => (
        <button
          key={item.id}
          type="button"
          role="tab"
          id={`${id}-${item.id}`}
          aria-selected={tab === item.id}
          aria-controls={`${id}-${item.id}-panel`}
          tabIndex={tab === item.id ? 0 : -1}
          className={tab === item.id ? "seg seg-on" : "seg"}
          onClick={() => onSelect(item.id)}
          onKeyDown={onKeyDown}
        >
          {item.label}
        </button>
      ))}
    </div>
  );
}

function titleOf(profile: Profile): string {
  const display = profile.display;
  return display && !display.masked && display.value ? display.value : `Person ${shortId(profile.id)}`;
}

function ledeOf(profile: Profile): string {
  const since = `In the workspace since ${formatDateTime(profile.created_at)}.`;
  const display = profile.display;
  if (!display) {
    return `No identifier yet. ${since}`;
  }
  return `${kindLabel(display.kind)}${display.masked ? ", hidden from you" : ""}. ${since}`;
}

function IdentityRow({ identity }: { identity: Identity }) {
  return (
    <li className="row">
      <span className="faint">{kindLabel(identity.kind)}</span>
      <span className="strong identity-value">{identity.masked ? <HiddenValue /> : identity.value}</span>
    </li>
  );
}

function IdentitiesPanel({ profile }: { profile: Profile }) {
  const empty = profile.identity_groups.length === 0 && profile.observed.length === 0;
  return (
    <Panel title="Identities">
      {empty ? (
        <EmptyState title="No identifiers">Nothing identifies this person yet.</EmptyState>
      ) : (
        <div className="stack">
          {profile.identity_groups.map((group) => (
            <section key={group.source} aria-label={`Identities from ${group.source}`}>
              <h3 className="strong">From {group.source}</h3>
              <ul className="list-plain stack-tight">
                {group.identities.map((identity, index) => (
                  <IdentityRow key={`${identity.kind}-${index}`} identity={identity} />
                ))}
              </ul>
            </section>
          ))}
          {profile.observed.length > 0 ? (
            <section aria-label="Identities seen on events">
              <h3 className="strong">Seen on events</h3>
              <ul className="list-plain stack-tight">
                {profile.observed.map((identity, index) => (
                  <IdentityRow key={`${identity.kind}-${index}`} identity={identity} />
                ))}
              </ul>
            </section>
          ) : null}
        </div>
      )}
    </Panel>
  );
}

function valueText(value: unknown): string {
  if (Array.isArray(value)) return value.map(valueText).join(", ");
  return typeof value === "string" ? value : JSON.stringify(value);
}

function TraitsPanel({ traits }: { traits: ProfileTrait[] }) {
  return (
    <Panel title="Fields" flush>
      {traits.length === 0 ? (
        <EmptyState title="No field values yet">Sources fill these in as they send what they know.</EmptyState>
      ) : (
        <div className="table-scroll">
          <table className="table">
            <caption>Field values with the source and time of each</caption>
            <thead>
              <tr>
                <th scope="col">Field</th>
                <th scope="col">Value</th>
                <th scope="col">Source</th>
                <th scope="col">Observed</th>
              </tr>
            </thead>
            <tbody>
              {traits.map((item) => (
                <tr key={item.key}>
                  <td>
                    <div className="strong">{item.key}</div>
                    {item.trait_type ? <div className="faint">{typeLabel(item.trait_type)}</div> : null}
                  </td>
                  <td className="identity-value">{item.masked ? <HiddenValue /> : valueText(item.value)}</td>
                  <td>{item.source ?? <span className="faint">—</span>}</td>
                  <td>{item.observed_at ? formatDateTime(item.observed_at) : <span className="faint">—</span>}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </Panel>
  );
}

function MergesPanel({ workspaceId, profile }: { workspaceId: string; profile: Profile }) {
  return (
    <Panel title="Merged people">
      <ul className="list-plain stack-tight">
        {profile.merges.map((merge) => (
          <li className="row" key={merge.merged_person_id}>
            <Link to={`/w/${workspaceId}/people/${merge.merged_person_id}`}>
              Person <span className="mono">{shortId(merge.merged_person_id)}</span>
            </Link>
            <span className="faint">merged in {formatDateTime(merge.merged_at)}</span>
          </li>
        ))}
      </ul>
    </Panel>
  );
}
