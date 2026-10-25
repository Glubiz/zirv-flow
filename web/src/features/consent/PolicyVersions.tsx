import { client, unwrap, useResource } from "../../api";
import { EmptyState, formatDateTime, Panel, ResourceView } from "../../ui";
import { describeCondition } from "../segments";

interface PolicyVersionsProps {
  workspaceId: string;
  /** Changes when a version is published, so the list reads again. */
  published: number | null;
}

/** The published versions of the policy, newest first, each with its rules in order. */
export function PolicyVersions({ workspaceId, published }: PolicyVersionsProps) {
  const versions = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/consent/policy/versions", { params: { path: { ws: workspaceId } } })),
    [workspaceId, published],
  );

  return (
    <Panel title="Version history" flush>
      <ResourceView
        resource={versions}
        loadingLabel="Loading the versions"
        isEmpty={(data) => data.length === 0}
        empty={<EmptyState title="Nothing published yet">Until a version is published, only the built-in recorded consent rule applies to a send.</EmptyState>}
      >
        {(data) => (
          <ol className="policy-versions" aria-label="Published versions, newest first">
            {data.map((version) => (
              <li key={version.version}>
                <details open={version.version === data[0]?.version}>
                  <summary>
                    <span className="strong">Version {version.version}</span> · {version.rules.length}{" "}
                    {version.rules.length === 1 ? "rule" : "rules"} · published {formatDateTime(version.published_at)}
                  </summary>
                  <ol className="policy-version-rules">
                    {version.rules.map((rule) => (
                      <li key={rule.id}>
                        <span className="strong">{rule.name}</span>
                        <span className="faint">
                          {rule.builtin
                            ? " Matches when the recorded consent does not allow the send."
                            : rule.condition
                              ? ` ${describeCondition(withoutVersion(rule.condition))}`
                              : ""}
                        </span>
                      </li>
                    ))}
                  </ol>
                </details>
              </li>
            ))}
          </ol>
        )}
      </ResourceView>
    </Panel>
  );
}

function withoutVersion(condition: unknown): Parameters<typeof describeCondition>[0] {
  const copy: Record<string, unknown> = { ...(condition as Record<string, unknown>) };
  delete copy["v"];
  return copy as unknown as Parameters<typeof describeCondition>[0];
}
