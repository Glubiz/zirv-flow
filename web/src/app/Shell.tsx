import { useEffect, useRef } from "react";
import { Link, NavLink, Outlet, useLocation } from "react-router";

import { AccountMenu, useSignedInUser } from "../features/session";
import { PrivacyNav } from "../features/privacy";
import { useCurrentWorkspace, WorkspaceSwitcher } from "../features/workspaces";
import { Brand } from "../ui";

const TITLES: Record<string, string> = {
  w: "Workspaces",
  members: "Members",
  traits: "Fields",
  people: "People",
  imports: "Imports",
  sources: "Sources",
  contacts: "Contacts",
  "contact-definition": "Contact definition",
  events: "Events",
  lists: "Lists",
  topics: "Topics",
  suppressions: "Suppressions",
  privacy: "Privacy",
  "api-keys": "API keys",
  traces: "Traces",
  jobs: "Jobs",
  health: "Health",
};

function titleFor(pathname: string): string {
  const parts = pathname.split("/").filter(Boolean);
  if (parts[2] === "people" && parts[3] === "conflicts") return "Identity conflicts";
  const key = parts[0] === "w" && parts.length > 2 ? (parts[2] ?? "w") : (parts[0] ?? "w");
  return TITLES[key] ?? "zirv flow";
}

function WorkspaceItem({
  base,
  segment,
  children,
  except,
}: {
  base: string | null;
  segment: string;
  children: string;
  /** A path below this item that belongs to another item. */
  except?: string;
}) {
  const location = useLocation();
  if (base === null) {
    return (
      <span className="nav-disabled" aria-disabled="true" title="Pick a workspace first">
        {children}
        <span className="visually-hidden"> (pick a workspace first)</span>
      </span>
    );
  }
  const to = `${base}/${segment}`;
  if (except !== undefined && location.pathname.startsWith(`${base}/${except}`)) {
    return <Link to={to}>{children}</Link>;
  }
  return <NavLink to={to}>{children}</NavLink>;
}

export function Shell() {
  const user = useSignedInUser();
  const current = useCurrentWorkspace();
  const location = useLocation();
  const main = useRef<HTMLElement>(null);
  const first = useRef(true);

  useEffect(() => {
    document.title = `${titleFor(location.pathname)} · zirv flow`;
    if (first.current) {
      first.current = false;
      return;
    }
    main.current?.focus();
  }, [location.pathname]);

  const base = current ? `/w/${current.id}` : null;

  return (
    <>
      <header className="header">
        <div className="header-inner">
          <Link to="/w" className="brand-link">
            <Brand />
          </Link>
          <nav aria-label="Main" className="nav">
            <NavLink to="/w" end>
              Workspaces
            </NavLink>
            <WorkspaceItem base={base} segment="members">Members</WorkspaceItem>
            <WorkspaceItem base={base} segment="traits">Fields</WorkspaceItem>
            <WorkspaceItem base={base} segment="lists">Lists</WorkspaceItem>
            <WorkspaceItem base={base} segment="contacts">Contacts</WorkspaceItem>
            <WorkspaceItem base={base} segment="people" except="people/conflicts">People</WorkspaceItem>
            <WorkspaceItem base={base} segment="people/conflicts">Conflicts</WorkspaceItem>
            <WorkspaceItem base={base} segment="sources">Sources</WorkspaceItem>
            <WorkspaceItem base={base} segment="imports">Imports</WorkspaceItem>
            <WorkspaceItem base={base} segment="events">Events</WorkspaceItem>
            <WorkspaceItem base={base} segment="topics">Topics</WorkspaceItem>
            <WorkspaceItem base={base} segment="suppressions">Suppressions</WorkspaceItem>
            {current ? <PrivacyNav workspaceId={current.id} /> : null}
            <WorkspaceItem base={base} segment="api-keys">API keys</WorkspaceItem>
            <WorkspaceItem base={base} segment="traces">Traces</WorkspaceItem>
            {user.is_owner ? <NavLink to="/jobs">Jobs</NavLink> : null}
            <NavLink to="/health">Health</NavLink>
          </nav>
          <div className="header-tools">
            <WorkspaceSwitcher />
            <AccountMenu />
          </div>
        </div>
      </header>
      <main id="main" ref={main} tabIndex={-1}>
        <Outlet />
      </main>
    </>
  );
}
