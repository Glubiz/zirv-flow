import { client, unwrap, useResource, type components, type Resource } from "../../api";

import type { EventSchemaDetail, SchemaProperty } from "./labels";

export type EventDefinition = components["schemas"]["EventSchemaBody"];

/** The workspace's event catalog: the events that have a schema. */
export function useEventCatalog(workspaceId: string): Resource<EventDefinition[]> {
  return useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/event-schemas", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
}

/** The properties a filter is typed by: the latest published version, else the draft. */
export function propertiesOf(detail: EventSchemaDetail | null | undefined): SchemaProperty[] {
  return detail?.versions[0]?.properties ?? detail?.draft ?? [];
}

/** The property names of one event, or an empty list while no event is chosen. */
export function useEventProperties(workspaceId: string, name: string): Resource<string[]> {
  return useResource(async () => {
    if (name === "") return [];
    const detail = await unwrap(
      client.GET("/api/v1/workspaces/{ws}/event-schemas/{name}", { params: { path: { ws: workspaceId, name } } }),
    );
    return propertiesOf(detail).map((property) => property.name);
  }, [workspaceId, name]);
}
