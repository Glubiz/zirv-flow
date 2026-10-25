import { client, unwrap, useResource, type components, type Resource } from "../../api";

export type EventDefinition = components["schemas"]["EventSchemaBody"];

/** The workspace's event catalog: the events that have a schema. */
export function useEventCatalog(workspaceId: string): Resource<EventDefinition[]> {
  return useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/event-schemas", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
}

/** The property names of one event, or an empty list while no event is chosen. */
export function useEventProperties(workspaceId: string, name: string): Resource<string[]> {
  return useResource(async () => {
    if (name === "") return [];
    const detail = await unwrap(
      client.GET("/api/v1/workspaces/{ws}/event-schemas/{name}", { params: { path: { ws: workspaceId, name } } }),
    );
    const properties = detail.versions[0]?.properties ?? detail.draft ?? [];
    return properties.map((property) => property.name);
  }, [workspaceId, name]);
}
