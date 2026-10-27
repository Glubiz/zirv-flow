/** The groups of the action menu, in the order they are shown. */
export const GROUPS = [
  "Sign-in",
  "Members and keys",
  "Credentials",
  "Publishing",
  "Imports and exports",
  "Privacy",
  "Settings",
] as const;

export const OTHER_GROUP = "Other";

export type Group = (typeof GROUPS)[number] | typeof OTHER_GROUP;

/** Which group each catalogue area belongs to. An area missing here lands in "Other". */
const AREA_GROUPS: Record<string, (typeof GROUPS)[number]> = {
  session: "Sign-in",
  user: "Sign-in",
  membership: "Members and keys",
  api_key: "Members and keys",
  credential: "Credentials",
  secret: "Credentials",
  secrets: "Credentials",
  webhook: "Credentials",
  webhook_signing: "Credentials",
  flow: "Publishing",
  template: "Publishing",
  theme: "Publishing",
  snippet: "Publishing",
  segment: "Publishing",
  list: "Publishing",
  asset: "Publishing",
  destination: "Publishing",
  import: "Imports and exports",
  source: "Imports and exports",
  sync_run: "Imports and exports",
  events: "Imports and exports",
  ingest_rejects: "Imports and exports",
  audit: "Imports and exports",
  stream: "Imports and exports",
  privacy: "Privacy",
  consent: "Privacy",
  consent_dimension: "Privacy",
  consent_policy: "Privacy",
  suppression: "Privacy",
  topic: "Privacy",
  person: "Privacy",
  contact: "Privacy",
  identity_conflict: "Privacy",
  workspace: "Settings",
  ai: "Settings",
  send_settings: "Settings",
  sender_identity: "Settings",
  sending_domain: "Settings",
  test_send: "Settings",
  forwarding: "Settings",
  event_policy: "Settings",
  event_schema: "Settings",
  contact_definition: "Settings",
  trait: "Settings",
  job: "Settings",
};

export function groupOf(area: string): Group {
  return AREA_GROUPS[area] ?? OTHER_GROUP;
}

/** The catalogue carries codes only, so a readable label is made from the code: "flow.enrollment_retried" reads "Flow enrollment retried". */
export function actionLabel(code: string): string {
  const words = code.replace(/[._]+/g, " ").trim();
  return words === "" ? code : words.charAt(0).toUpperCase() + words.slice(1);
}

export interface AreaEntry {
  area: string;
  actions: string[];
}

export interface GroupedActions {
  group: Group;
  areas: AreaEntry[];
}

/** The catalogue areas sorted into the seven groups, then "Other" when anything is left; empty groups are dropped. */
export function groupAreas(areas: readonly AreaEntry[]): GroupedActions[] {
  const sorted = [...areas].sort((a, b) => a.area.localeCompare(b.area));
  const order: Group[] = [...GROUPS, OTHER_GROUP];
  return order
    .map((group) => ({ group, areas: sorted.filter((entry) => groupOf(entry.area) === group) }))
    .filter((entry) => entry.areas.length > 0);
}
