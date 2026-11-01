import { useId, useState } from "react";

import { client, unwrap, useResource } from "../../api";
import { EmptyState, formatDateTime, Pill, ResourceView, Sheet, shortId } from "../../ui";
import { formatMicros } from "./money";

interface UsagePanelProps {
  workspaceId: string;
  /** Changes when the settings do, so the budget shown is reloaded with them. */
  version: string;
}

type Usage = Awaited<ReturnType<typeof loadUsage>>;

function loadUsage(workspaceId: string) {
  return unwrap(client.GET("/api/v1/workspaces/{ws}/ai/usage", { params: { path: { ws: workspaceId } } }));
}

export function UsagePanel({ workspaceId, version }: UsagePanelProps) {
  const usage = useResource(() => loadUsage(workspaceId), [workspaceId, version]);
  const [everyCall, setEveryCall] = useState(false);
  const headingId = useId();

  return (
    <section className="panel panel-pad" aria-labelledby={headingId}>
      <div className="row">
        <h2 className="panel-title row-grow" id={headingId}>
          Usage this month
        </h2>
        {usage.status === "ready" && usage.data.recent.length > 0 ? (
          <button type="button" className="link-button" onClick={() => setEveryCall(true)}>
            Every call
          </button>
        ) : null}
      </div>
      <ResourceView
        resource={usage}
        loadingLabel="Loading usage"
        isEmpty={(data) => data.recent.length === 0 && data.by_feature.length === 0}
        empty={<EmptyState title="No calls this month">Calls the assistant makes appear here, without their content.</EmptyState>}
      >
        {(data) => (
          <div className="stack">
            <div className="table-scroll">
              <table className="table">
                <caption className="visually-hidden">Spend by feature</caption>
                <thead>
                  <tr>
                    <th scope="col">Feature</th>
                    <th scope="col" className="num">
                      Calls
                    </th>
                    <th scope="col" className="num">
                      Cost
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {data.by_feature.map((row) => (
                    <tr key={row.feature}>
                      <td>{row.feature}</td>
                      <td className="num mono">{row.calls}</td>
                      <td className="num mono">{formatMicros(row.cost_micros)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
            {everyCall ? (
              <Sheet title="Every call" modal onClose={() => setEveryCall(false)}>
                <EveryCall data={data} />
              </Sheet>
            ) : null}
          </div>
        )}
      </ResourceView>
    </section>
  );
}

function EveryCall({ data }: { data: Usage }) {
  return (
    <div className="stack">
      <div className="table-scroll">
        <table className="table">
          <caption>Spend by user</caption>
          <thead>
            <tr>
              <th scope="col">User</th>
              <th scope="col">Calls</th>
              <th scope="col">Cost</th>
            </tr>
          </thead>
          <tbody>
            {data.by_user.map((row) => (
              <tr key={row.user_id ?? "api-key"}>
                <td className="mono">{row.user_id ? shortId(row.user_id) : "API key"}</td>
                <td className="num">{row.calls}</td>
                <td className="num">{formatMicros(row.cost_micros)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <div className="table-scroll">
        <table className="table">
          <caption>Recent calls</caption>
          <thead>
            <tr>
              <th scope="col">When</th>
              <th scope="col">Feature</th>
              <th scope="col">Model</th>
              <th scope="col">Tokens in / out</th>
              <th scope="col">Cost</th>
              <th scope="col">Result</th>
            </tr>
          </thead>
          <tbody>
            {data.recent.map((call) => (
              <tr key={call.id}>
                <td>{formatDateTime(call.created_at)}</td>
                <td>{call.feature}</td>
                <td className="mono">{call.model}</td>
                <td className="num">
                  {call.input_tokens} / {call.output_tokens}
                </td>
                <td className="num">{formatMicros(call.estimated_cost_micros)}</td>
                <td>
                  {call.outcome === "ok" ? (
                    <Pill>OK</Pill>
                  ) : call.outcome === "pending" || call.outcome === "abandoned" ? (
                    <Pill>{call.outcome}</Pill>
                  ) : (
                    <Pill tone="error">{call.outcome}</Pill>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}
