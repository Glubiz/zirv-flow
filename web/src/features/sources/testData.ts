import type { Freshness, Run, RunDetail, Source, SourceKind, Stream } from "./labels";

/** The fake source type as `GET /api/v1/source-kinds` lists it, for the tests of this feature. */
export const FAKE_KIND: SourceKind = {
  kind: "fake",
  label: "Fake source",
  description: "An in-memory source for tests.",
  config_fields: [
    {
      key: "streams",
      label: "Streams",
      help: "How many streams discovery reports.",
      kind: "number",
      options: [],
      required: false,
      secret: false,
    },
    {
      key: "fail_check",
      label: "Fail the check",
      help: "Makes the connection test fail.",
      kind: "bool",
      options: [],
      required: false,
      secret: false,
    },
  ],
  credential_fields: [
    {
      key: "token",
      label: "Token",
      help: "The check passes only for the token ok.",
      kind: "text",
      options: [],
      required: true,
      secret: true,
    },
  ],
  capabilities: { incremental: true, full_refresh: true, emits_deletes: false },
};

export function aSource(change: Partial<Source> = {}): Source {
  return {
    id: "src-1",
    kind: "fake",
    name: "CRM",
    config: { streams: 2 },
    credential_fields_set: ["token"],
    schedule: null,
    enabled: true,
    freshness: null,
    created_by: null,
    created_at: "2026-10-01T09:00:00Z",
    updated_at: "2026-10-02T09:00:00Z",
    ...change,
  };
}

export function aStream(change: Partial<Stream> = {}): Stream {
  return {
    id: "str-1",
    name: "stream_1",
    enabled: false,
    sync_mode: "full_refresh",
    cursor_field: "updated_at",
    primary_key: ["id"],
    staleness_threshold_seconds: null,
    freshness: null,
    mapping: null,
    discovered: {
      fields: [
        { name: "id", field_type: "number" },
        { name: "name", field_type: "string" },
        { name: "updated_at", field_type: "timestamp" },
      ],
      sync_modes: ["full_refresh", "incremental"],
      default_cursor_field: "updated_at",
      default_primary_key: ["id"],
    },
    updated_at: "2026-10-02T09:00:00Z",
    ...change,
  };
}

export function aRun(change: Partial<Run> = {}): Run {
  return {
    id: "run-1",
    source_id: "src-1",
    stream_id: "str-1",
    stream: "stream_1",
    state: "succeeded",
    trigger: "manual",
    sync_mode: "full_refresh",
    records_read: 7,
    records_applied: 5,
    records_unchanged: 0,
    records_rejected: 1,
    records_deleted: 1,
    reject_threshold: 0.05,
    error_code: null,
    attempt: 1,
    trace_id: "0123456789abcdef0123456789abcdef",
    created_by: null,
    created_at: "2026-10-03T09:00:00Z",
    started_at: "2026-10-03T09:00:01Z",
    finished_at: "2026-10-03T09:00:09Z",
    ...change,
  };
}

export function aRunDetail(change: Partial<RunDetail> = {}): RunDetail {
  return {
    ...aRun(),
    rejects: [
      {
        id: "rej-1",
        received_at: "2026-10-03T09:00:05Z",
        reason: "invalid_identifier",
        reasons: [{ property: "email", reason: "invalid_identifier" }],
        sample: { properties: { name: "Ada", email: null }, masked: ["email"] } as never,
      },
    ],
    ...change,
  };
}

export function aFreshness(change: Partial<Freshness> = {}): Freshness {
  return {
    state: "fresh",
    last_success_at: null,
    high_water_mark: null,
    threshold_seconds: null,
    threshold_is_default: false,
    ...change,
  };
}
