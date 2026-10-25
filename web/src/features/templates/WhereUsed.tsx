import { Link } from "react-router";

import { client, unwrap, useResource } from "../../api";
import { EmptyState, Pill, ResourceView } from "../../ui";
import type { TemplateUse } from "./model";

export function useWhereUsed(workspaceId: string, templateId: string, reloadKey = 0) {
  return useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/templates/{id}/where-used", {
          params: { path: { ws: workspaceId, id: templateId } },
        }),
      ),
    [workspaceId, templateId, reloadKey],
  );
}

export function UseList({ workspaceId, uses }: { workspaceId: string; uses: readonly TemplateUse[] }) {
  return (
    <ul className="list-plain" aria-label="Flows that use this template">
      {uses.map((use) => (
        <li key={use.flow_id} className="row">
          <Link to={`/w/${workspaceId}/flows/${use.flow_id}`}>{use.flow_name}</Link>
          <Pill tone={use.active ? "attention" : "neutral"}>{use.active ? "Active" : "Not running now"}</Pill>
          <span className="faint">
            {use.flow_state}, {use.versions.length === 1 ? "version" : "versions"} {use.versions.join(", ")}
          </span>
        </li>
      ))}
    </ul>
  );
}

/** The where-used tab: every flow with a version that names the template. */
export function WhereUsedView({ workspaceId, templateId, reloadKey }: { workspaceId: string; templateId: string; reloadKey: number }) {
  const uses = useWhereUsed(workspaceId, templateId, reloadKey);
  return (
    <ResourceView
      resource={uses}
      loadingLabel="Loading where this template is used"
      isEmpty={(data) => data.length === 0}
      empty={<EmptyState title="No flow uses this template">Choose it in a send step of a flow and it shows here.</EmptyState>}
    >
      {(data) => <UseList workspaceId={workspaceId} uses={data} />}
    </ResourceView>
  );
}
