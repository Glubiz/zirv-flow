import { Link } from "react-router";

import { EmptyState, Pill, ResourceView, type Tone } from "../../ui";
import { USER_LABEL, usersOf, type ThemeUser } from "./model";
import { useThemeUsedBy } from "./useThemes";

const TONE: Record<ThemeUser["kind"], Tone> = { named: "neutral", default: "neutral", flow: "attention" };

/** What uses a theme, each linked and typed. */
export function ThemeUserList({ users, label = "What uses this theme" }: { users: readonly ThemeUser[]; label?: string }) {
  return (
    <ul className="list-plain" aria-label={label}>
      {users.map((user) => (
        <li key={`${user.kind}:${user.id}`} className="row">
          <Link to={user.href}>{user.name}</Link>
          <Pill tone={TONE[user.kind]}>{USER_LABEL[user.kind]}</Pill>
        </li>
      ))}
    </ul>
  );
}

const GROUPS: readonly { kind: ThemeUser["kind"]; title: string; none: string }[] = [
  { kind: "named", title: "Templates that name this theme", none: "No template names this theme." },
  { kind: "default", title: "Templates that follow the default", none: "No template follows the default through this theme." },
  { kind: "flow", title: "Active flows", none: "No active flow sends these templates." },
];

/** The where-used tab: templates that name the theme, templates that follow the default, and active flows. */
export function WhereUsedView({ workspaceId, themeId, reloadKey }: { workspaceId: string; themeId: string; reloadKey: number }) {
  const used = useThemeUsedBy(workspaceId, themeId, reloadKey);
  return (
    <ResourceView
      resource={used}
      loadingLabel="Loading where this theme is used"
      isEmpty={(data) => usersOf(workspaceId, data).length === 0}
      empty={<EmptyState title="Nothing uses this theme">Pick it in a template, or make it the default, and it shows here.</EmptyState>}
    >
      {(data) => {
        const users = usersOf(workspaceId, data);
        return (
          <div className="stack">
            {GROUPS.map((group) => {
              const list = users.filter((user) => user.kind === group.kind);
              return (
                <section key={group.kind} className="stack-tight" aria-label={group.title}>
                  <h3 className="panel-title">{group.title}</h3>
                  {list.length === 0 ? <p className="faint">{group.none}</p> : <ThemeUserList users={list} label={group.title} />}
                </section>
              );
            })}
          </div>
        );
      }}
    </ResourceView>
  );
}
