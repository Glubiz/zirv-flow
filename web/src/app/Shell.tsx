import { useEffect, useRef, useState } from "react";
import { Link, Outlet, useLocation, useNavigate } from "react-router";

import { useWorkspaceAccess } from "../features/members";
import { AccountMenu, useSignedInUser } from "../features/session";
import { useCurrentWorkspace, WorkspaceSwitcher, type Workspace } from "../features/workspaces";
import { Brand, Button, MenuButton, Sheet } from "../ui";
import { destinationUrl, isSettingsPath, matchesDestination, navigationGroups, NAVIGATION, visibleDestinations, type Destination } from "./navigation";
import { NavigationIcon } from "./NavigationIcon";
import { PagePalette } from "./PagePalette";
import { useSidebarFold } from "./useSidebarFold";

export interface ShellContext {
  destinations: Destination[];
}

export function Shell() {
  const current = useCurrentWorkspace();
  const user = useSignedInUser();
  const access = useWorkspaceAccess(current?.id ?? null);
  const destinations = visibleDestinations({ canAdmin: current !== null && (user.is_owner || (access.status === "ready" && access.data.canAdmin)), isOwner: user.is_owner });
  const canSearchPeople = current !== null && (user.is_owner || (access.status === "ready" && access.data.role !== null));
  return <ShellLayout current={current} destinations={destinations} canSearchPeople={canSearchPeople} />;
}

function ShellLayout({ current, destinations, canSearchPeople }: { current: Workspace | null; destinations: Destination[]; canSearchPeople: boolean }) {
  const location = useLocation();
  const navigate = useNavigate();
  const main = useRef<HTMLElement>(null);
  const first = useRef(true);
  const { folded, toggle } = useSidebarFold(location.pathname);
  const [drawerPath, setDrawerPath] = useState<string | null>(null);
  const [paletteOpen, setPaletteOpen] = useState(false);
  const base = current ? `/w/${current.id}` : null;
  const settings = isSettingsPath(location.pathname, base);
  const drawerOpen = drawerPath === location.pathname;
  if (drawerPath !== null && !drawerOpen) setDrawerPath(null);

  useEffect(() => {
    const page = [...NAVIGATION].sort((a, b) => b.route.length - a.route.length).find((item) => matchesDestination(item, location.pathname, base));
    document.title = `${page?.label ?? "zirv flow"} · zirv flow`;
  }, [location.pathname, base]);

  useEffect(() => {
    if (first.current) {
      first.current = false;
      return;
    }
    main.current?.focus();
  }, [location.pathname]);

  useEffect(() => {
    function shortcut(event: KeyboardEvent) {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        setDrawerPath(null);
        setPaletteOpen((open) => !open);
      }
    }
    document.addEventListener("keydown", shortcut);
    return () => document.removeEventListener("keydown", shortcut);
  }, []);

  function search() {
    setDrawerPath(null);
    setPaletteOpen(true);
  }

  function itemLink(item: Destination, label = item.label, icon = false) {
    const to = destinationUrl(item, base);
    const content = icon ? <NavigationIcon name={label} /> : label;
    return to === null ? (
      <span key={item.route} className="sidebar-link" aria-disabled="true" aria-label={icon ? label : undefined} title="Pick a workspace first">
        {content}<span className="visually-hidden"> (pick a workspace first)</span>
      </span>
    ) : (
      <Link key={item.route} className="sidebar-link" to={to} aria-label={icon ? label : undefined} title={icon ? label : undefined}
        aria-current={matchesDestination(item, location.pathname, base) ? "page" : undefined} onClick={() => setDrawerPath(null)}>
        {content}
      </Link>
    );
  }

  function sidebar(rail: boolean) {
    // Instance-scoped settings (Jobs, Health) stay reachable from the main menu while no workspace is picked.
    const items = destinations.filter((item) => item.area === (settings ? "settings" : "main") || (base === null && !settings && item.global && item.area === "settings"));
    const groups = navigationGroups(items);
    const overview = destinations.find((item) => item.route === "settings");
    return (
      <nav className={rail ? "sidebar-content sidebar-rail" : "sidebar-content"} aria-label={settings ? "Settings" : "Main"}>
        <div className="sidebar-brand">
          <Link to="/w" className="brand-link" aria-label="Workspaces" aria-current={location.pathname === "/w" ? "page" : undefined} onClick={() => setDrawerPath(null)}><Brand /></Link>
          {!rail ? <Button className="sidebar-fold" aria-label="Fold the menu" onClick={toggle}><NavigationIcon name="Fold" /></Button> : null}
        </div>
        <WorkspaceSwitcher compact={rail} />
        <Button className="sidebar-search" aria-label="Search pages" aria-keyshortcuts="Meta+K Control+K" onClick={search}>
          {rail ? <NavigationIcon name="Search" /> : <>Search <kbd>⌘K</kbd></>}
        </Button>
        <div className="sidebar-nav">
          {settings ? (
            <>
              <Link className="sidebar-link" to={base ?? "/w"} aria-label={`Back to ${current?.name ?? "workspaces"}`} title={rail ? `Back to ${current?.name ?? "workspaces"}` : undefined} onClick={() => setDrawerPath(null)}>
                <NavigationIcon name="Back" />{rail ? null : <span>Back to {current?.name ?? "workspaces"}</span>}
              </Link>
              {overview ? itemLink(overview, rail ? "Settings" : "Overview", rail) : null}
            </>
          ) : items.filter((item) => !item.group).map((item) => itemLink(item, item.label, rail))}
          {groups.map((group) => {
            const entries = items.filter((item) => item.group === group);
            const active = entries.some((item) => matchesDestination(item, location.pathname, base));
            return rail ? (
              <MenuButton key={group} label={group} placement="beside" current={active}
                buttonClassName="sidebar-group-button"
                buttonContent={<NavigationIcon name={group} />}
                entries={entries.map((item) => {
                  const to = destinationUrl(item, base);
                  return { key: item.route, label: item.label, disabled: to === null, reason: to === null ? "Pick a workspace first" : undefined,
                    onSelect: () => { if (to !== null) void navigate(to); } };
                })} />
            ) : (
              <div className="sidebar-group" key={group}>
                <div className="sidebar-group-label">{group}</div>
                {entries.map((item) => itemLink(item))}
              </div>
            );
          })}
        </div>
        <div className="sidebar-bottom">
          {!settings && overview ? itemLink(overview, "Settings", rail) : null}
          {rail ? <Button aria-label="Unfold the menu" onClick={toggle}><NavigationIcon name="Unfold" /></Button> : null}
          <div className="sidebar-account"><AccountMenu /></div>
        </div>
      </nav>
    );
  }

  return (
    <div className={folded ? "app-shell app-shell-folded" : "app-shell"}>
      <header className="mobile-bar">
        <Button aria-label="Open menu" aria-expanded={drawerOpen} onClick={() => setDrawerPath(location.pathname)}><NavigationIcon name="Menu" /></Button>
        <Link to={base ?? "/w"} className="brand-link"><Brand /></Link>
        <Button aria-label="Search pages" onClick={search}><NavigationIcon name="Search" /></Button>
      </header>
      <aside className="sidebar">{sidebar(folded)}</aside>
      <main id="main" ref={main} tabIndex={-1}><Outlet context={{ destinations } satisfies ShellContext} /></main>
      {drawerOpen ? <Sheet title="Navigation" closeLabel="Close menu" modal onClose={() => setDrawerPath(null)}>{sidebar(false)}</Sheet> : null}
      {paletteOpen ? <PagePalette items={destinations} base={base} workspaceId={current?.id ?? null} canSearchPeople={canSearchPeople} onClose={() => setPaletteOpen(false)} /> : null}
    </div>
  );
}
