import { client, unwrap, useResource } from "../../api";
import { EmptyState, formatDateTime, Panel, Pill, ResourceView, shortId } from "../../ui";
import { formatMicros } from "./money";

interface UsagePanelProps {
  workspaceId: string;
  /** Changes when the settings do, so the budget shown is reloaded with them. */
  version: string;
}

export function UsagePanel({ workspaceId, version }: UsagePanelProps) {
  const usage = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/ai/usage", { params: { path: { ws: workspaceId } } })),
    [workspaceId, version],
  );

  return (
    <Panel title="Usage this month">
      <ResourceView
        resource={usage}
        loadingLabel="Loading usage"
        isEmpty={(data) => data.recent.length === 0 && data.by_feature.length === 0}
        empty={<EmptyState title="No calls this month">Calls the assistant makes appear here, without their content.</EmptyState>}
      >
        {(data) => (
          <div className="stack">
            <p>
              <span className="strong">{formatMicros(data.spent_micros)}</span> of{" "}
              <span className="strong">{formatMicros(data.budget_micros)}</span> spent
              {data.budget_micros === 0 ? " — no budget is set, so the assistant makes no calls" : ""}
            </p>
            <progress
              aria-label="Budget used"
              max={Math.max(data.budget_micros, 1)}
              value={Math.min(data.spent_micros, Math.max(data.budget_micros, 1))}
            />
            <div className="table-scroll">
              <table className="table">
                <caption>Spend by feature</caption>
                <thead>
                  <tr>
                    <th scope="col">Feature</th>
                    <th scope="col">Calls</th>
                    <th scope="col">Input tokens</th>
                    <th scope="col">Output tokens</th>
                    <th scope="col">Cost</th>
                  </tr>
                </thead>
                <tbody>
                  {data.by_feature.map((row) => (
                    <tr key={row.feature}>
                      <td className="strong">{row.feature}</td>
                      <td className="num">{row.calls}</td>
                      <td className="num">{row.input_tokens}</td>
                      <td className="num">{row.output_tokens}</td>
                      <td className="num">{formatMicros(row.cost_micros)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
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
                        ) : call.outcome === "pending" ? (
                          <Pill>pending</Pill>
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
        )}
      </ResourceView>
    </Panel>
  );
}
