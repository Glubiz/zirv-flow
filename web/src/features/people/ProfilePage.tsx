import { Link, useParams } from "react-router";

import { client, unwrap, useResource, type components } from "../../api";
import { EmptyState, formatDateTime, HiddenValue, kindLabel, LoadError, LoadingState, PageHeader, Panel, shortId, TabNav, useTabParam } from "../../ui";
import { ConsentPanel } from "../consent";
import { PersonListsPanel } from "../lists";
import { useWorkspaceAccess } from "../members";
import { SegmentExplainPanel } from "../segments";
import { MessageLog } from "./MessageLog";
import { Timeline } from "./Timeline";
import { sourceName, useWorkspaceSources } from "./sourceOrder";
import { typeLabel } from "./TraitDialog";

type Profile = components["schemas"]["ProfileBody"];
type SourceBody = components["schemas"]["SourceBody"];
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
  const { active: tab } = useTabParam(TABS);
  const access = useWorkspaceAccess(workspaceId);
  const sources = useWorkspaceSources(workspaceId);
  const sourceList = sources.status === "ready" ? sources.data : null;
  const canWrite = access.status === "ready" && access.data.canEdit;

  const crumb = { group: "Audience", section: "People", to: `/w/${workspaceId}/people` };

  if (profile.status === "loading") {
    return (
      <div className="page">
        <PageHeader title="Person" breadcrumb={crumb} />
        <LoadingState label="Loading the profile" />
      </div>
    );
  }
  if (profile.status === "error") {
    return (
      <div className="page">
        <PageHeader title="Person" breadcrumb={crumb} />
        <Panel flush>
          <LoadError error={profile.error} onRetry={profile.reload} noun="person" />
        </Panel>
      </div>
    );
  }
  const data = profile.data;

  return (
    <div className="page">
      <PageHeader title={titleOf(data)} breadcrumb={crumb} lede={ledeOf(data)} />
      {data.merged_from ? (
        <p className="notice" role="status">
          The person you opened, {shortId(data.merged_from)}, was merged into this one. This is the combined profile.
        </p>
      ) : null}
      <TabNav label="Person sections" tabs={TABS} />
      {tab === "messages" ? (
        <MessageLog workspaceId={workspaceId} personId={data.id} />
      ) : (
        <div className="stack">
          <div className="profile-layout">
            <div className="stack">
              <IdentitiesPanel profile={data} sources={sourceList} />
              <TraitsPanel traits={data.traits} sources={sourceList} />
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

const TABS = [
  { id: "profile", label: "Profile" },
  { id: "messages", label: "Messages" },
];

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

function IdentitiesPanel({ profile, sources }: { profile: Profile; sources: SourceBody[] | null }) {
  const empty = profile.identity_groups.length === 0 && profile.observed.length === 0;
  return (
    <Panel title="Identities">
      {empty ? (
        <EmptyState title="No identifiers">Nothing identifies this person yet.</EmptyState>
      ) : (
        <div className="stack">
          {profile.identity_groups.map((group) => (
            <section key={group.source} aria-label={`Identities from ${sourceName(group.source, sources)}`}>
              <h3 className="strong">From {sourceName(group.source, sources)}</h3>
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

function TraitsPanel({ traits, sources }: { traits: ProfileTrait[]; sources: SourceBody[] | null }) {
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
                  <td>{item.source ? sourceName(item.source, sources) : <span className="faint">—</span>}</td>
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
