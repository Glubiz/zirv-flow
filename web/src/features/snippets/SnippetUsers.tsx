import { Link } from "react-router";

import { EmptyState, Pill, ResourceView, type Tone } from "../../ui";
import { USER_LABEL, usersOf, type SnippetUser } from "./model";
import { useSnippetUsedBy } from "./useSnippets";

const TONE: Record<SnippetUser["kind"], Tone> = { template: "neutral", theme: "neutral", flow: "attention" };

/** What uses a snippet, each linked and typed. */
export function SnippetUserList({ users, label = "What uses this snippet" }: { users: readonly SnippetUser[]; label?: string }) {
  return (
    <ul className="list-plain" aria-label={label}>
      {users.map((user) => (
        <li key={`${user.kind}:${user.id}`} className="row">
          <Link to={user.href}>{user.name}</Link>
          <Pill tone={TONE[user.kind]}>{USER_LABEL[user.kind]}</Pill>
          {user.where ? <span className="faint">in the {user.where}</span> : null}
        </li>
      ))}
    </ul>
  );
}

/** The where-used tab: the templates, themes and active flows that use the snippet. */
export function WhereUsedView({ workspaceId, snippetId, reloadKey }: { workspaceId: string; snippetId: string; reloadKey: number }) {
  const used = useSnippetUsedBy(workspaceId, snippetId, reloadKey);
  return (
    <ResourceView
      resource={used}
      loadingLabel="Loading where this snippet is used"
      isEmpty={(data) => usersOf(workspaceId, data).length === 0}
      empty={<EmptyState title="Nothing uses this snippet">Add it to a template with a snippet block and it shows here.</EmptyState>}
    >
      {(data) => <SnippetUserList users={usersOf(workspaceId, data)} />}
    </ResourceView>
  );
}
