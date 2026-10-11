import type { CsvImport } from "./labels";

/** An import as the API returns it, for the tests of this feature. */
export function anImport(change: Partial<CsvImport> = {}): CsvImport {
  return {
    id: "imp-1",
    name: "Customers May",
    source: "csv",
    stream: "customers-may",
    file_name: "customers.csv",
    file_sha256: "ab".repeat(32),
    size_bytes: 120,
    delimiter: ",",
    header: ["Email", "Plan", "Newsletter"],
    mapping: null,
    status: "uploaded",
    rows_total: 3,
    rows_done: 0,
    rows_applied: 0,
    rows_unchanged: 0,
    rows_rejected: 0,
    reject_threshold: 0.05,
    error_code: null,
    created_by: null,
    trace_id: "0".repeat(32),
    created_at: "2026-10-01T09:00:00Z",
    updated_at: "2026-10-01T09:00:00Z",
    started_at: null,
    finished_at: null,
    ...change,
  };
}

export const SAMPLES = [
  ["ada@example.test", "pro", "yes"],
  ["bob@example.test", "free", "no"],
];

export const TRAITS = [
  {
    key: "plan",
    type: "string",
    archived: false,
    channel_address: false,
    created_at: "2026-10-01T09:00:00Z",
    description: "",
    inferred: false,
    pii: false,
    precedence: "latest",
    source_order: [],
    updated_at: "2026-10-01T09:00:00Z",
  },
  {
    key: "old_plan",
    type: "string",
    archived: true,
    channel_address: false,
    created_at: "2026-10-01T09:00:00Z",
    description: "",
    inferred: false,
    pii: false,
    precedence: "latest",
    source_order: [],
    updated_at: "2026-10-01T09:00:00Z",
  },
];

export const LISTS = [
  {
    id: "list-1",
    name: "Newsletter",
    description: "",
    member_count: 0,
    created_at: "2026-10-01T09:00:00Z",
    updated_at: "2026-10-01T09:00:00Z",
    created_by: null,
  },
];
