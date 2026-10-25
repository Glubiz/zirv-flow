import { client, unwrap, useResource, type components, type Resource } from "../../api";

export type FieldDefinition = components["schemas"]["TraitBody"];

/** The workspace's field registry: the traits a person can have. */
export function useFieldRegistry(workspaceId: string): Resource<FieldDefinition[]> {
  return useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/traits", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
}
