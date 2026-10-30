import { Fragment } from "react";

import { client, unwrap, useResource, type components, type Resource } from "../../api";
import { EmptyState, reasonLabel, ResourceView } from "../../ui";

export type VersionCounts = components["schemas"]["VersionCountsBody"];

export function useVersionCounts(workspaceId: string, flowId: string, version: number | null) {
  return useResource(async (): Promise<VersionCounts | null> => {
    if (version === null) return null;
    return unwrap(client.GET("/api/v1/workspaces/{ws}/flows/{id}/versions/{n}/counts", {
      params: { path: { ws: workspaceId, id: flowId, n: version } },
    }));
  }, [workspaceId, flowId, version]);
}

export function StepResults({ counts, nodeId }: { counts: Resource<VersionCounts | null>; nodeId: string }) {
  return (
    <ResourceView resource={counts} loadingLabel="Loading step results">
      {(data) => {
        if (!data) return <EmptyState title="No published results yet">Publish a version before people can enter this flow.</EmptyState>;
        const node = data.nodes.find((entry) => entry.node_id === nodeId);
        return (
          <div className="stack">
            <p className="strong">Results from version {data.version}</p>
            <p className="faint">These are the published step&apos;s results. Draft changes have not run.</p>
            {node ? (
              <dl className="fl-step-results">
                <dt>Reached (step runs)</dt><dd>{node.entered.toLocaleString("en")}</dd>
                <dt>Completed</dt><dd>{node.completed.toLocaleString("en")}</dd>
                <dt>Waiting</dt><dd>{node.waiting.toLocaleString("en")}</dd>
                <dt>Failed step runs</dt><dd>{node.failed.toLocaleString("en")}</dd>
                <dt>Messages sent</dt><dd>{node.sent.toLocaleString("en")}</dd>
                <dt>Messages failed</dt><dd>{node.send_failed.toLocaleString("en")}</dd>
                <dt>Goal conversions</dt><dd>{node.goal_conversions.toLocaleString("en")}</dd>
                {node.skipped.map((skip) => <Fragment key={skip.reason}>
                  <dt>Skipped: {reasonLabel(skip.reason)}</dt><dd>{skip.count.toLocaleString("en")}</dd>
                </Fragment>)}
              </dl>
            ) : <p className="muted">This step is not in version {data.version}.</p>}
          </div>
        );
      }}
    </ResourceView>
  );
}
