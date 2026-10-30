export interface NavigationAccess {
  canAdmin: boolean;
  isOwner: boolean;
}

export interface Destination {
  label: string;
  route: string;
  area: "main" | "settings" | "pages" | "utility";
  group: string;
  role?: "admin" | "owner";
  global?: boolean;
  exact?: boolean;
  matches?: string[];
  except?: string[];
}

export const NAVIGATION: readonly Destination[] = [
  { label: "Home", route: "", area: "main", group: "", exact: true },
  { label: "People", route: "people", area: "main", group: "Audience" },
  { label: "Segments", route: "segments", area: "main", group: "Audience" },
  { label: "Lists", route: "lists", area: "main", group: "Audience" },
  { label: "Contacts", route: "contacts", area: "main", group: "Audience" },
  { label: "Consent", route: "topics", area: "main", group: "Audience", matches: ["topics", "consent", "suppressions"] },
  { label: "Sources", route: "sources", area: "main", group: "Data" },
  { label: "Imports", route: "imports", area: "main", group: "Data" },
  { label: "Events", route: "events", area: "main", group: "Data", except: ["events/explorer", "events/settings"] },
  { label: "Flows", route: "flows", area: "main", group: "Messaging" },
  { label: "Templates", route: "templates", area: "main", group: "Messaging" },
  { label: "Content library", route: "themes", area: "main", group: "Messaging", matches: ["themes", "snippets", "images"] },
  { label: "Explorer", route: "events/explorer", area: "main", group: "Insights" },
  { label: "Traces", route: "traces", area: "main", group: "Insights" },
  { label: "Settings", route: "settings", area: "utility", group: "", exact: true },
  { label: "Members and roles", route: "members", area: "settings", group: "Workspace" },
  { label: "Fields", route: "traits", area: "settings", group: "Workspace" },
  { label: "Contact definition", route: "contact-definition", area: "settings", group: "Workspace" },
  { label: "Sending domains", route: "sending-domains", area: "settings", group: "Sending", role: "admin" },
  { label: "Destinations", route: "destinations", area: "settings", group: "Sending", role: "admin" },
  { label: "Sending limits", route: "settings/sending", area: "settings", group: "Sending" },
  { label: "Forwarding", route: "forwarding", area: "settings", group: "Sending", role: "admin" },
  { label: "Event settings", route: "events/settings", area: "settings", group: "Events" },
  { label: "API keys", route: "api-keys", area: "settings", group: "Integrations", role: "admin" },
  { label: "Credentials", route: "credentials", area: "settings", group: "Integrations", role: "admin" },
  { label: "AI assistant", route: "ai", area: "settings", group: "Integrations", role: "admin" },
  { label: "Privacy and erasure", route: "privacy", area: "settings", group: "Compliance", role: "admin" },
  { label: "Audit log", route: "settings/audit", area: "settings", group: "Compliance", role: "admin" },
  { label: "Jobs", route: "/jobs", area: "settings", group: "System", role: "owner", global: true },
  { label: "Health", route: "/health", area: "settings", group: "System", global: true },
  { label: "Identity conflicts", route: "people/conflicts", area: "pages", group: "Audience" },
  { label: "Consent dimensions", route: "consent/dimensions", area: "pages", group: "Audience" },
  { label: "Consent policy", route: "consent/policy", area: "pages", group: "Audience" },
  { label: "Suppressions", route: "suppressions", area: "pages", group: "Audience" },
  { label: "Event debugger", route: "events/debugger", area: "pages", group: "Data" },
  { label: "Ingest rejects", route: "events/rejects", area: "pages", group: "Data" },
  { label: "Snippets", route: "snippets", area: "pages", group: "Messaging" },
  { label: "Images", route: "images", area: "pages", group: "Messaging" },
  { label: "Workspaces", route: "/w", area: "utility", group: "", global: true, exact: true },
];

export function visibleDestinations(access: NavigationAccess): Destination[] {
  return NAVIGATION.filter((item) => !item.role || (item.role === "owner" ? access.isOwner : access.canAdmin));
}

export function destinationUrl(item: Destination, base: string | null): string | null {
  return item.global ? item.route : base === null ? null : `${base}${item.route ? `/${item.route}` : ""}`;
}

function below(path: string, route: string): boolean {
  return path === route || path.startsWith(`${route}/`);
}

export function matchesDestination(item: Destination, pathname: string, base: string | null): boolean {
  const url = destinationUrl(item, base);
  if (url === null) return false;
  const path = pathname.replace(/\/$/, "");
  const prefix = base ?? "";
  if (item.exact) return path === url;
  if (item.except?.some((route) => below(path, `${prefix}/${route}`))) return false;
  return item.matches ? item.matches.some((route) => below(path, `${prefix}/${route}`)) : below(path, url);
}

export function isSettingsPath(pathname: string, base: string | null): boolean {
  return NAVIGATION.some((item) => (item.area === "settings" || item.route === "settings") && matchesDestination(item, pathname, base));
}

export function navigationGroups(items: readonly Destination[]): string[] {
  return [...new Set(items.map((item) => item.group).filter(Boolean))];
}

export function isFlowEditor(pathname: string): boolean {
  return /^\/w\/[^/]+\/flows\/(?!new\/?$)[^/]+\/?$/.test(pathname);
}
