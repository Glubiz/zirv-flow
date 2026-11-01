import { useState, type SyntheticEvent } from "react";
import { Link, useParams } from "react-router";

import { client, describeError, unwrap, useResource, type ErrorSummary } from "../../api";
import {
  Button,
  EmptyState,
  InlineError,
  PageHeader,
  Panel,
  ResourceView,
  SelectField,
  TextField,
  formatDateTime,
} from "../../ui";
import { BarChart } from "./BarChart";
import { EventsNav } from "./EventsNav";
import { FilterBuilder, buildFilter, typeOf, type BuiltFilter, type FilterRow } from "./FilterBuilder";
import { PropertyFacts } from "./PropertyFacts";
import type { SchemaProperty } from "./labels";

const RANGES: { value: string; label: string; hours: number }[] = [
  { value: "24h", label: "Last 24 hours", hours: 24 },
  { value: "7d", label: "Last 7 days", hours: 24 * 7 },
  { value: "30d", label: "Last 30 days", hours: 24 * 30 },
  { value: "90d", label: "Last 90 days", hours: 24 * 90 },
];

const BUCKETS: { value: string; label: string }[] = [
  { value: "hour", label: "Hour" },
  { value: "day", label: "Day" },
  { value: "week", label: "Week" },
];

/** What the results show: the query as it was run. */
interface Query {
  name: string;
  filters: BuiltFilter[];
  from: string;
  to: string;
  person: string;
  bucket: string;
  property: string;
}

interface Form {
  name: string;
  range: string;
  bucket: string;
  person: string;
  property: string;
}

const DEFAULT_FORM: Form = { name: "", range: "7d", bucket: "day", person: "", property: "" };

function bounds(range: string): { from: string; to: string } {
  const hours = RANGES.find((option) => option.value === range)?.hours ?? 24 * 7;
  const now = Date.now();
  return { from: new Date(now - hours * 3_600_000).toISOString(), to: new Date(now + 60_000).toISOString() };
}

function scopeBody(query: Query) {
  return {
    ...(query.name !== "" ? { name: query.name } : {}),
    filters: query.filters.map((filter) => ({ key: filter.key, op: filter.op, value: filter.value })),
    from: query.from,
    to: query.to,
    ...(query.person !== "" ? { person_id: query.person } : {}),
  };
}

export function ExplorerPage() {
  const { ws = "" } = useParams();

  return (
    <div className="page">
      <PageHeader
        title="Event explorer" breadcrumb={{ group: "Insights", section: "Explorer" }}
        lede="Count, break down and list the stored events of a time range. Every query is bounded by time and cut off if it runs long."
      />
      <EventsNav workspaceId={ws} />
      <Explorer key={ws} workspaceId={ws} />
    </div>
  );
}

function Explorer({ workspaceId }: { workspaceId: string }) {
  const schemas = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/event-schemas", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const [form, setForm] = useState<Form>(DEFAULT_FORM);
  const [rows, setRows] = useState<FilterRow[]>([]);
  const [problem, setProblem] = useState<string | null>(null);
  const [applied, setApplied] = useState<Query>(() => ({
    name: "",
    filters: [],
    ...bounds(DEFAULT_FORM.range),
    person: "",
    bucket: DEFAULT_FORM.bucket,
    property: "",
  }));
  const [run, setRun] = useState(0);

  // The schema of the chosen event gives the filters their types.
  const detail = useResource(
    () =>
      form.name === ""
        ? Promise.resolve(null)
        : unwrap(client.GET("/api/v1/workspaces/{ws}/event-schemas/{name}", { params: { path: { ws: workspaceId, name: form.name } } })),
    [workspaceId, form.name],
  );
  const properties: SchemaProperty[] =
    detail.status === "ready" ? (detail.data?.versions[0]?.properties ?? []) : [];

  function submit(event: SyntheticEvent) {
    event.preventDefault();
    const filters: BuiltFilter[] = [];
    for (const row of rows) {
      const built = buildFilter(row, typeOf(properties, row.key.trim()));
      if (typeof built === "string") {
        setProblem(built);
        return;
      }
      filters.push(built);
    }
    setProblem(null);
    setApplied({
      name: form.name,
      filters,
      ...bounds(form.range),
      person: form.person.trim(),
      bucket: form.bucket,
      property: form.property.trim(),
    });
    setRun((value) => value + 1);
  }

  return (
    <>
      <form className="stack" onSubmit={submit} aria-label="Explorer query">
        <div className="inline-form">
          <SelectField
            label="Event"
            value={form.name}
            onChange={(event) => {
              setForm({ ...form, name: event.target.value, property: "" });
              setRows([]);
            }}
          >
            <option value="">Any event</option>
            {schemas.status === "ready"
              ? schemas.data.map((schema) => (
                  <option key={schema.name} value={schema.name}>
                    {schema.name}
                  </option>
                ))
              : null}
          </SelectField>
          <SelectField label="Time range" value={form.range} onChange={(event) => setForm({ ...form, range: event.target.value })}>
            {RANGES.map((option) => (
              <option key={option.value} value={option.value}>
                {option.label}
              </option>
            ))}
          </SelectField>
          <SelectField label="Chart by" value={form.bucket} onChange={(event) => setForm({ ...form, bucket: event.target.value })}>
            {BUCKETS.map((option) => (
              <option key={option.value} value={option.value}>
                {option.label}
              </option>
            ))}
          </SelectField>
          <TextField
            label="Person id"
            mono
            hint="Optional. Includes the people merged into them."
            value={form.person}
            onChange={(event) => setForm({ ...form, person: event.target.value })}
            autoComplete="off"
          />
          <TextField
            label="Break down by property"
            list="explorer-properties"
            hint="Optional."
            value={form.property}
            onChange={(event) => setForm({ ...form, property: event.target.value })}
            autoComplete="off"
          />
        </div>
        <FilterBuilder properties={properties} rows={rows} onChange={setRows} />
        {problem ? <InlineError title={problem} /> : null}
        <div className="row">
          <Button type="submit" variant="primary">
            Run query
          </Button>
        </div>
      </form>
      <Results key={`${run}`} workspaceId={workspaceId} query={applied} />
    </>
  );
}

function Results({ workspaceId, query }: { workspaceId: string; query: Query }) {
  const [after, setAfter] = useState<string | null>(null);

  const count = useResource(
    () =>
      unwrap(
        client.POST("/api/v1/workspaces/{ws}/events/explore/count", {
          params: { path: { ws: workspaceId } },
          body: { ...scopeBody(query), bucket: query.bucket },
        }),
      ),
    [workspaceId, JSON.stringify(query)],
  );
  const unit = query.bucket;

  return (
    <>
      <Panel>
        <h2 className="panel-title">Events over time</h2>
        <ResourceView
          resource={count}
          loadingLabel="Counting events"
          isEmpty={(data) => data.buckets.length === 0}
          empty={<EmptyState title="No events match">Try a wider time range or fewer filters.</EmptyState>}
        >
          {(data) => (
            <>
              <p className="muted">
                {data.total} {data.total === 1 ? "event" : "events"} in the range.
              </p>
              {data.truncated ? (
                <p className="notice" role="status">
                  More {unit}s matched than can be shown. The newest are left out; narrow the time range.
                </p>
              ) : null}
              <BarChart bars={data.buckets.map((bucket) => ({ start: bucket.bucket_start, count: bucket.count }))} unit={unit} />
            </>
          )}
        </ResourceView>
      </Panel>
      {query.property !== "" ? <Breakdown workspaceId={workspaceId} query={query} /> : null}
      <Rows workspaceId={workspaceId} query={query} after={after} onAfter={setAfter} />
    </>
  );
}

function Breakdown({ workspaceId, query }: { workspaceId: string; query: Query }) {
  const breakdown = useResource(
    () =>
      unwrap(
        client.POST("/api/v1/workspaces/{ws}/events/explore/breakdown", {
          params: { path: { ws: workspaceId } },
          body: { ...scopeBody(query), property: query.property },
        }),
      ),
    [workspaceId, JSON.stringify(query)],
  );

  return (
    <Panel flush>
      <h2 className="panel-title">
        Values of <span className="mono">{query.property}</span>
      </h2>
      <ResourceView
        resource={breakdown}
        loadingLabel="Counting values"
        isEmpty={(data) => data.values.length === 0 && data.missing === 0}
        empty={<EmptyState title="No events match" />}
      >
        {(data) => (
          <div className="table-scroll">
            <table className="table">
              <caption>Most common values of {query.property}</caption>
              <thead>
                <tr>
                  <th scope="col">Value</th>
                  <th scope="col" className="num">
                    Events
                  </th>
                </tr>
              </thead>
              <tbody>
                {data.values.map((value) => (
                  <tr key={value.value}>
                    <td className="mono">{value.value}</td>
                    <td className="num">{value.count}</td>
                  </tr>
                ))}
                {data.other > 0 ? (
                  <tr>
                    <td className="faint">Other values</td>
                    <td className="num">{data.other}</td>
                  </tr>
                ) : null}
                {data.missing > 0 ? (
                  <tr>
                    <td className="faint">Without the property</td>
                    <td className="num">{data.missing}</td>
                  </tr>
                ) : null}
              </tbody>
            </table>
          </div>
        )}
      </ResourceView>
    </Panel>
  );
}

function Rows({
  workspaceId,
  query,
  after,
  onAfter,
}: {
  workspaceId: string;
  query: Query;
  after: string | null;
  onAfter: (cursor: string | null) => void;
}) {
  const page = useResource(
    () =>
      unwrap(
        client.POST("/api/v1/workspaces/{ws}/events/explore/list", {
          params: { path: { ws: workspaceId } },
          body: { ...scopeBody(query), ...(after !== null ? { cursor: after } : {}) },
        }),
      ),
    [workspaceId, JSON.stringify(query), after],
  );
  const [exporting, setExporting] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function exportCsv() {
    setExporting(true);
    setFailure(null);
    try {
      const blob = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/events/explore/export", {
          params: { path: { ws: workspaceId } },
          body: scopeBody(query),
          parseAs: "blob",
        }),
      );
      const url = URL.createObjectURL(blob);
      const link = document.createElement("a");
      link.href = url;
      link.download = "events.csv";
      link.click();
      URL.revokeObjectURL(url);
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setExporting(false);
    }
  }

  return (
    <Panel flush>
      <div className="panel-pad row">
        <h2 className="panel-title row-grow">Events</h2>
        <Button onClick={() => void exportCsv()} disabled={exporting}>
          {exporting ? "Exporting" : "Export CSV"}
        </Button>
      </div>
      {failure ? (
        <div className="panel-pad">
          <InlineError {...failure} />
        </div>
      ) : null}
      <ResourceView
        resource={page}
        loadingLabel="Loading events"
        isEmpty={(data) => data.events.length === 0}
        empty={<EmptyState title="No events match" />}
      >
        {(data) => (
          <>
            <div className="table-scroll">
              <table className="table">
                <caption>Matching events</caption>
                <thead>
                  <tr>
                    <th scope="col">Occurred</th>
                    <th scope="col">Event</th>
                    <th scope="col">Source</th>
                    <th scope="col">State</th>
                    <th scope="col">Properties</th>
                  </tr>
                </thead>
                <tbody>
                  {data.events.map((event) => (
                    <tr key={event.id}>
                      <td>{formatDateTime(event.occurred_at)}</td>
                      <td className="strong">{event.name}</td>
                      <td>{event.source}</td>
                      <td>
                        {event.validation_state}
                        {event.flags.length > 0 ? <span className="faint"> {event.flags.join(", ")}</span> : null}
                      </td>
                      <td>
                        <details className="sample">
                          <summary>
                            Show properties <span className="visually-hidden">of the {event.name} event</span>
                          </summary>
                          <PropertyFacts properties={event.properties} masked={event.masked} />
                          <p>
                            <Link to={`/w/${workspaceId}/people/${event.person_id}`}>Open the person</Link>
                            {" · "}
                            <Link to={`/w/${workspaceId}/traces/${event.trace_id}`}>Open the trace</Link>
                          </p>
                        </details>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
            <div className="panel-pad row">
              {after !== null ? <Button onClick={() => onAfter(null)}>Back to the newest</Button> : null}
              {data.next_cursor ? <Button onClick={() => onAfter(data.next_cursor ?? null)}>Older events</Button> : null}
            </div>
          </>
        )}
      </ResourceView>
    </Panel>
  );
}
