import { client, unwrap, useResource } from "../../api";
import { EmptyState, RelatedPanel, ResourceView, type RelatedGroup } from "../../ui";
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

const FLOW_STATE_LABEL: Record<string, string> = { published: "Live", draft: "Draft", paused: "Paused", archived: "Archived" };

export function usedByFlows(workspaceId: string, uses: readonly TemplateUse[]): RelatedGroup[] {
  return [
    {
      label: "Used by flows",
      items: uses.map((use) => ({
        key: use.flow_id,
        label: use.flow_name,
        to: `/w/${workspaceId}/flows/${use.flow_id}`,
        meta: FLOW_STATE_LABEL[use.flow_state] ?? use.flow_state,
      })),
    },
  ];
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
      {(data) => <RelatedPanel groups={usedByFlows(workspaceId, data)} />}
    </ResourceView>
  );
}
