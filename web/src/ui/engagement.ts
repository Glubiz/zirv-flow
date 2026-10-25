/** The built-in open event: some mail clients and gateways fetch the pixel without a person looking. */
export const OPENED_EVENT = "email.opened";

/** What `machine = false` means; shown beside the filter that choosing `email.opened` adds. */
export const MACHINE_HINT = "not detected as machine-generated";

export interface MachineFilter {
  prop: "machine";
  op: "eq";
  value: false;
}

/** The property filter that leaves out opens detected as machine-generated. */
export function machineFilter(): MachineFilter {
  return { prop: "machine", op: "eq", value: false };
}

/** The filters a freshly chosen event starts with: the machine filter for an open, none otherwise. */
export function defaultEventFilters(event: string): MachineFilter[] {
  return event === OPENED_EVENT ? [machineFilter()] : [];
}

/** Whether a filter is the machine filter, so the hint that explains it belongs beside it. */
export function isMachineFilter(filter: { prop: string; op: string; value?: unknown }): boolean {
  return filter.prop === "machine" && filter.op === "eq" && filter.value === false;
}

/** The properties of the built-in engagement events, for the event schemas that do not list them. */
export const ENGAGEMENT_PROPERTIES: Readonly<Record<string, Readonly<Record<string, "boolean" | "string">>>> = {
  [OPENED_EVENT]: { machine: "boolean", machine_reason: "string" },
};
