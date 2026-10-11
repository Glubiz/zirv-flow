import { Link } from "react-router";

import { client, unwrap, useResource } from "../../api";
import { EmptyState, formatDateTime, Panel, ResourceView } from "../../ui";
import { sourceLabel } from "./access";

interface PersonListsPanelProps {
  workspaceId: string;
  personId: string;
}

/** The lists a person is in, each linking to the list. */
export function PersonListsPanel({ workspaceId, personId }: PersonListsPanelProps) {
  const lists = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/people/{id}/lists", {
          params: { path: { ws: workspaceId, id: personId } },
        }),
      ),
    [workspaceId, personId],
  );

  return (
    <Panel title="Lists">
      <ResourceView
        resource={lists}
        loadingLabel="Loading lists"
        isEmpty={(data) => data.length === 0}
        empty={<EmptyState title="On no list">Add this person to a list from the list's page.</EmptyState>}
      >
        {(data) => (
          <ul className="list-plain stack-tight">
            {data.map((list) => (
              <li className="row" key={list.list_id}>
                <Link to={`/w/${workspaceId}/lists/${list.list_id}`}>{list.name}</Link>
                <span className="faint">
                  {sourceLabel(list.source)} · {formatDateTime(list.added_at)}
                </span>
              </li>
            ))}
          </ul>
        )}
      </ResourceView>
    </Panel>
  );
}
