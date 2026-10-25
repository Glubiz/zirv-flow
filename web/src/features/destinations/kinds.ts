import { client, unwrap, useResource, type Resource } from "../../api";
import { kindFromSpec, type DestinationKind } from "./labels";

/** The destination types this build ships, with their fields, from `/destination-kinds`. */
export function useDestinationKinds(): Resource<DestinationKind[]> {
  return useResource(async () => (await unwrap(client.GET("/api/v1/destination-kinds"))).map(kindFromSpec), []);
}

/** The loaded kinds, or none yet: for labels that fall back to the raw type id. */
export function loadedKinds(resource: Resource<DestinationKind[]>): readonly DestinationKind[] {
  return resource.status === "ready" ? resource.data : [];
}
