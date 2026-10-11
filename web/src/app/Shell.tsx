import { useEffect, useRef } from "react";
import { Link, NavLink, Outlet, useLocation } from "react-router";

import { AccountMenu, useSignedInUser } from "../features/session";
import { useCurrentWorkspace, WorkspaceSwitcher } from "../features/workspaces";
import { Brand } from "../ui";

const TITLES: Record<string, string> = {
  w: "Workspaces",
  members: "Members",
  "api-keys": "API keys",
  traces: "Traces",
  jobs: "Jobs",
  health: "Health",
};

function titleFor(pathname: string): string {
  const parts = pathname.split("/").filter(Boolean);
  const key = parts[0] === "w" && parts.length > 2 ? (parts[2] ?? "w") : (parts[0] ?? "w");
  return TITLES[key] ?? "zirv flow";
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
            <NavLink to={base ? `${base}/members` : "/w"} end={!base}>
              Members
            </NavLink>
            <NavLink to={base ? `${base}/api-keys` : "/w"} end={!base}>
              API keys
            </NavLink>
            <NavLink to={base ? `${base}/traces` : "/w"} end={!base}>
              Traces
            </NavLink>
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
